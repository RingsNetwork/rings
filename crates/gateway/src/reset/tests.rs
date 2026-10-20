//! The reset law, checked on the classifier's whole domain and on a model of the captured-flow
//! path explored exhaustively.
//!
//! # Model
//!
//! One flow, with the ground truth of its circuit next to what the client endpoint observes:
//!
//! ```text
//! Path = (stage, hole, unseen, failed, end)
//!   stage   Handshake | Opening | Open
//!   hole    None | Delayed | Lost      a frame missing while a later one is held (observed)
//!   unseen  None | Forward | Return    a frame lost with no later frame of its direction
//!                                      (observed by no one; a Return loss is a reply tail
//!                                      with the world's fin, caught at Δ = 2V)
//!   failed  None | Some(c)             a failure the probe reads
//!   end     None | Orderly | Reset(r)
//! σ(p) = Failed(c) if failed = c,  Stalled if hole ≠ None,  Live otherwise
//! ```
//!
//! Actions, enabled while `end = None`:
//!
//! ```text
//! Establish        Handshake → Opening          OpenOk   Opening → Open
//! OpenErr          Opening: end bridge(Opening, OnionUnavailable)
//! Delay | Lose     Open, hole = None: hole ← Delayed | Lost
//! Arrive           hole = Delayed: hole ← None
//! GapExpires       hole ≠ None: failed ← gap   (missing for V since a later frame came)
//! LoseUnseen(d)    Open, unseen = None: unseen ← d
//! Deadline         unseen = Return: failed ← silent   (no frame for Δ)
//! Abort            Open: failed ← abort   (h's abort, a link fact, a failed pump)
//! Eof              Open: failed ⇒ end eof(σ) (report before end);
//!                  hole = None ∧ unseen ≠ Return ⇒ the world's fin: end eof(σ) or Orderly
//! ClientSilent     end closure(φ, ClientSilent)     ClientReset  end closure(φ, ClientReset)
//! BridgeIo         Open: end bridge(φ, StreamIo)
//! ```
//!
//! Every action of the path changes one variable once, so the reachable space is finite and the
//! breadth-first search below visits all of it.

use super::*;
use crate::FlowId;

/// The flow the errors of these tests name.
fn flow() -> FlowId {
    FlowId {
        source: "100.64.0.2:41000".parse().expect("test source"),
        target: "93.184.216.34:443".parse().expect("test target"),
    }
}

/// Every condition, with one failure cause.
fn conditions() -> [OnionStreamCondition; 3] {
    [
        OnionStreamCondition::Live,
        OnionStreamCondition::Stalled,
        OnionStreamCondition::Failed("cause".to_string()),
    ]
}

/// Every phase of the classifier's domain.
fn phases() -> Vec<FlowPhase> {
    [FlowPhase::Handshake, FlowPhase::Opening]
        .into_iter()
        .chain(conditions().into_iter().map(FlowPhase::Open))
        .collect()
}

/// Every endpoint closure.
const CLOSURES: [EndpointClosure; 3] = [
    EndpointClosure::ClientReset,
    EndpointClosure::ClientSilent,
    EndpointClosure::Ungraceful,
];

/// One bridge error of each class `bridge` tells apart.
fn bridge_errors() -> [GatewayError; 3] {
    [
        GatewayError::StreamIo {
            flow: flow(),
            operation: "write",
            message: "broken pipe".to_string(),
        },
        GatewayError::OnionUnavailable {
            target: flow().target,
            message: "no route".to_string(),
        },
        GatewayError::platform("bridge", "closed"),
    ]
}

/// Law Idle on the whole domain: `closure(φ, κ) = IdleTimeout ⇔ κ = ClientSilent ∧ φ =
/// Open(Live)`.
#[test]
fn test_idle_timeout_iff_silent_client_on_a_live_open_stream() {
    for phase in phases() {
        for closure in CLOSURES {
            let idle = closure_reason(&phase, closure) == FlowResetReason::IdleTimeout;
            let live_and_silent = closure == EndpointClosure::ClientSilent
                && phase == FlowPhase::Open(OnionStreamCondition::Live);
            assert_eq!(idle, live_and_silent, "{phase:?} closed by {closure:?}");
        }
    }
}

/// Law Dominance on the whole domain: a failed stream names every end `StreamReset(c)`.
#[test]
fn test_an_observed_failure_dominates_every_local_cause() {
    let cause = "cause".to_string();
    let phase = FlowPhase::Open(OnionStreamCondition::Failed(cause.clone()));
    let named = FlowResetReason::StreamReset(cause.clone());
    for closure in CLOSURES {
        assert_eq!(closure_reason(&phase, closure), named);
    }
    for error in bridge_errors() {
        assert_eq!(bridge_reason(&phase, &error), named);
    }
    assert_eq!(eof_reason(OnionStreamCondition::Failed(cause)), Some(named));
}

/// Law Phase: an expired clock names what was waiting.
#[test]
fn test_an_expired_clock_names_what_was_waiting() {
    let named = [
        (FlowPhase::Handshake, FlowResetReason::HandshakeExpired),
        (FlowPhase::Opening, FlowResetReason::OpenExpired),
        (
            FlowPhase::Open(OnionStreamCondition::Stalled),
            FlowResetReason::StreamStalled,
        ),
    ];
    for (phase, reason) in named {
        assert_eq!(
            closure_reason(&phase, EndpointClosure::ClientSilent),
            reason
        );
    }
}

/// Without a failure, local causes keep their own names, and only a failed stream's EOF resets.
#[test]
fn test_local_causes_keep_their_names_without_a_failure() {
    for phase in phases()
        .into_iter()
        .filter(|phase| !matches!(phase, FlowPhase::Open(OnionStreamCondition::Failed(_))))
    {
        assert_eq!(
            closure_reason(&phase, EndpointClosure::ClientReset),
            FlowResetReason::ClientReset
        );
        assert_eq!(
            closure_reason(&phase, EndpointClosure::Ungraceful),
            FlowResetReason::UngracefulClose
        );
        let [io, unavailable, other] = bridge_errors();
        assert_eq!(
            bridge_reason(&phase, &io),
            FlowResetReason::StreamFailed("write: broken pipe".to_string())
        );
        assert_eq!(
            bridge_reason(&phase, &unavailable),
            FlowResetReason::OnionOpenFailed("no route".to_string())
        );
        assert_eq!(
            bridge_reason(&phase, &other),
            FlowResetReason::OnionOpenFailed(other.to_string())
        );
    }
    for condition in [OnionStreamCondition::Live, OnionStreamCondition::Stalled] {
        assert_eq!(eof_reason(condition), None);
    }
}

/// What the flow was waiting on, in the model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Handshake,
    Opening,
    Open,
}

/// A frame missing while a later one of its direction is held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Hole {
    None,
    /// The missing frame is late and still arrives.
    Delayed,
    /// The missing frame was lost.
    Lost,
}

/// The direction of a frame lost with no later frame of its direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Unseen {
    /// A client-to-`h` frame: `h` never learns of it, since credit carries no sequence.
    Forward,
    /// An `h`-to-client frame: the client's deadline `Δ` fails the session.
    Return,
}

/// How a model flow ended.
#[derive(Clone, Debug, Eq, PartialEq)]
enum End {
    /// The world's `fin` half-closed the flow.
    Orderly,
    /// The runtime reset the flow for this reason.
    Reset(FlowResetReason),
}

/// One state of the captured-flow path; see the module model.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Path {
    stage: Stage,
    hole: Hole,
    unseen: Option<Unseen>,
    failed: Option<&'static str>,
    end: Option<End>,
}

/// An action of the captured-flow path; see the module model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Establish,
    OpenOk,
    OpenErr,
    Delay,
    Lose,
    Arrive,
    GapExpires,
    LoseUnseen(Unseen),
    Deadline,
    Abort,
    Eof,
    ClientSilent,
    ClientReset,
    BridgeIo,
}

/// Every action of the model.
const ACTIONS: [Action; 15] = [
    Action::Establish,
    Action::OpenOk,
    Action::OpenErr,
    Action::Delay,
    Action::Lose,
    Action::Arrive,
    Action::GapExpires,
    Action::LoseUnseen(Unseen::Forward),
    Action::LoseUnseen(Unseen::Return),
    Action::Deadline,
    Action::Abort,
    Action::Eof,
    Action::ClientSilent,
    Action::ClientReset,
    Action::BridgeIo,
];

impl Path {
    /// The captured SYN.
    const fn captured() -> Self {
        Self {
            stage: Stage::Handshake,
            hole: Hole::None,
            unseen: None,
            failed: None,
            end: None,
        }
    }

    /// `σ`: what the probe reads.
    fn condition(&self) -> OnionStreamCondition {
        match (self.failed, self.hole) {
            (Some(cause), _) => OnionStreamCondition::Failed(cause.to_string()),
            (None, Hole::Delayed | Hole::Lost) => OnionStreamCondition::Stalled,
            (None, Hole::None) => OnionStreamCondition::Live,
        }
    }

    /// `φ`: what the runtime reads.
    fn phase(&self) -> FlowPhase {
        match self.stage {
            Stage::Handshake => FlowPhase::Handshake,
            Stage::Opening => FlowPhase::Opening,
            Stage::Open => FlowPhase::Open(self.condition()),
        }
    }

    /// Whether the circuit lost a frame no endpoint observed.
    const fn lost_unseen(&self) -> bool {
        self.unseen.is_some()
    }

    /// The state after `action`, or `None` where the action is not enabled.
    fn after(&self, action: Action) -> Option<Self> {
        if self.end.is_some() {
            return None;
        }
        let open = self.stage == Stage::Open;
        let healthy = open && self.failed.is_none();
        let mut next = self.clone();
        match action {
            Action::Establish if self.stage == Stage::Handshake => next.stage = Stage::Opening,
            Action::OpenOk if self.stage == Stage::Opening => next.stage = Stage::Open,
            Action::OpenErr if self.stage == Stage::Opening => {
                let error = GatewayError::OnionUnavailable {
                    target: flow().target,
                    message: "no route".to_string(),
                };
                next.end = Some(End::Reset(bridge_reason(&self.phase(), &error)));
            }
            Action::Delay if healthy && self.hole == Hole::None => next.hole = Hole::Delayed,
            Action::Lose if healthy && self.hole == Hole::None => next.hole = Hole::Lost,
            Action::Arrive if healthy && self.hole == Hole::Delayed => next.hole = Hole::None,
            Action::GapExpires if healthy && self.hole != Hole::None => next.failed = Some("gap"),
            Action::LoseUnseen(direction) if healthy && self.unseen.is_none() => {
                next.unseen = Some(direction);
            }
            Action::Deadline if healthy && self.unseen == Some(Unseen::Return) => {
                next.failed = Some("silent");
            }
            Action::Abort if healthy => next.failed = Some("abort"),
            Action::Eof if open && self.failed.is_some() => {
                next.end = eof_reason(self.condition()).map(End::Reset);
            }
            Action::Eof
                if healthy && self.hole == Hole::None && self.unseen != Some(Unseen::Return) =>
            {
                next.end = Some(eof_reason(self.condition()).map_or(End::Orderly, End::Reset));
            }
            Action::ClientSilent => {
                let reason = closure_reason(&self.phase(), EndpointClosure::ClientSilent);
                next.end = Some(End::Reset(reason));
            }
            Action::ClientReset => {
                let reason = closure_reason(&self.phase(), EndpointClosure::ClientReset);
                next.end = Some(End::Reset(reason));
            }
            Action::BridgeIo if open => {
                let error = GatewayError::StreamIo {
                    flow: flow(),
                    operation: "write",
                    message: "broken pipe".to_string(),
                };
                next.end = Some(End::Reset(bridge_reason(&self.phase(), &error)));
            }
            _ => return None,
        }
        Some(next)
    }
}

/// Every terminal state of the model with the action that ended it, by breadth-first search
/// from the captured SYN.
fn terminal_paths() -> Vec<(Path, Action, Path)> {
    let mut visited = vec![Path::captured()];
    let mut frontier = vec![Path::captured()];
    let mut terminals = Vec::new();
    while let Some(path) = frontier.pop() {
        for action in ACTIONS {
            let Some(next) = path.after(action) else {
                continue;
            };
            if next.end.is_some() {
                terminals.push((path.clone(), action, next));
            } else if !visited.contains(&next) {
                visited.push(next.clone());
                frontier.push(next);
            }
        }
    }
    terminals
}

/// Model check of the reset law on every execution of the captured-flow path.
///
/// - Idle: a reset is `IdleTimeout` iff the client was silent while the stream was open, and
///   no failure was observed nor a frame known missing.
/// - Dominance: an observed failure names the reset `StreamReset`, whatever ends the flow.
/// - Report before end: an EOF after a failure is never an orderly end.
/// - Residual: an idle timeout over a defective circuit is reachable only through an unseen
///   loss, and its twin without that loss, identical in every observation, is an idle timeout
///   too; the forward case reaches it, as the module documentation of `crate::reset` states.
#[test]
fn test_reset_law_holds_on_every_execution_of_the_captured_flow_model() {
    let terminals = terminal_paths();
    assert!(!terminals.is_empty());
    let mut unseen_idle = Vec::new();
    for (before, action, after) in &terminals {
        let idle = after.end == Some(End::Reset(FlowResetReason::IdleTimeout));
        let observably_healthy =
            before.stage == Stage::Open && before.failed.is_none() && before.hole == Hole::None;
        assert_eq!(
            idle,
            *action == Action::ClientSilent && observably_healthy,
            "{before:?} --{action:?}--> {after:?}"
        );
        if let Some(cause) = before.failed {
            assert_eq!(
                after.end,
                Some(End::Reset(FlowResetReason::StreamReset(cause.to_string()))),
                "{before:?} --{action:?}--> {after:?}"
            );
        }
        if idle && before.lost_unseen() {
            let twin = Path {
                unseen: None,
                ..before.clone()
            };
            assert_eq!(twin.phase(), before.phase());
            assert_eq!(
                twin.after(Action::ClientSilent).and_then(|path| path.end),
                Some(End::Reset(FlowResetReason::IdleTimeout))
            );
            unseen_idle.push(before.unseen);
        }
    }
    assert!(unseen_idle.contains(&Some(Unseen::Forward)));
}

/// The model's reasons are a function of the runtime's observations `(φ, action)` alone: two
/// executions the runtime cannot tell apart are named alike (observational equivalence).
#[test]
fn test_reset_reasons_are_a_function_of_the_observations() {
    let terminals = terminal_paths();
    for (before, action, after) in &terminals {
        for (other, other_action, other_after) in &terminals {
            if before.phase() == other.phase() && action == other_action {
                assert_eq!(after.end, other_after.end, "{before:?} and {other:?}");
            }
        }
    }
}
