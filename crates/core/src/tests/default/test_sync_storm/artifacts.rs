//! Stable replay and failure artifacts for sync-storm scenarios.

use super::*;

pub(super) type FailureState = std::rc::Rc<std::cell::RefCell<FailureDiagnostic>>;

pub(super) struct FailureDiagnostic {
    observed_events: usize,
    rolling_digest: sha2::Sha256,
    digest: String,
    snapshot: serde_json::Value,
    recent_events: serde_json::Value,
}

impl Default for FailureDiagnostic {
    fn default() -> Self {
        use sha2::Digest as _;

        Self {
            observed_events: 0,
            rolling_digest: sha2::Sha256::new(),
            digest: hex::encode(sha2::Sha256::digest([])),
            snapshot: serde_json::json!({"virtual_ms": 0}),
            recent_events: serde_json::json!([]),
        }
    }
}

impl FailureDiagnostic {
    pub(super) fn observe(&mut self, state: &SimState) {
        use sha2::Digest as _;

        let events = state.trace().events();
        for event in events.iter().skip(self.observed_events) {
            let encoded = serde_json::to_vec(event)
                .unwrap_or_else(|error| format!("trace-event-error:{error}").into_bytes());
            self.rolling_digest.update(encoded);
        }
        self.observed_events = events.len();
        self.digest = hex::encode(self.rolling_digest.clone().finalize());
        self.snapshot = serde_json::to_value(state.snapshot())
            .unwrap_or_else(|error| serde_json::json!({"snapshot_error": error.to_string()}));
        self.recent_events = serde_json::to_value(&events[events.len().saturating_sub(8)..])
            .unwrap_or_else(|error| serde_json::json!({"recent_error": error.to_string()}));
    }
}

pub(super) struct ScenarioFailureGuard<'a> {
    runtime: &'a SimulationRuntimeGuard,
    diagnostics: FailureState,
    armed: std::cell::Cell<bool>,
}

impl<'a> ScenarioFailureGuard<'a> {
    pub(super) fn new(runtime: &'a SimulationRuntimeGuard) -> Self {
        Self {
            runtime,
            diagnostics: std::rc::Rc::new(std::cell::RefCell::new(FailureDiagnostic::default())),
            armed: std::cell::Cell::new(true),
        }
    }

    pub(super) fn diagnostics(&self) -> FailureState {
        self.diagnostics.clone()
    }

    pub(super) fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for ScenarioFailureGuard<'_> {
    fn drop(&mut self) {
        if !self.armed.get() || !std::thread::panicking() {
            return;
        }
        let diagnostic = self.diagnostics.borrow();
        let failure = serde_json::json!({
            "scenario": self.runtime.artifact_context().unwrap_or_else(|error| {
                serde_json::json!({"context_error": error.to_string()})
            }),
            "runtime": runtime_replay_snapshot(self.runtime).unwrap_or_else(|error| {
                serde_json::json!({"runtime_error": error})
            }),
            "trace_digest": diagnostic.digest,
            "virtual_state": diagnostic.snapshot,
            "recent_events": diagnostic.recent_events,
        });
        eprintln!(
            "sync-storm failure boundary: {}",
            serde_json::to_string(&failure).unwrap_or_else(|error| error.to_string())
        );
        let _ = persist_failure_artifact(self.runtime, &failure);
    }
}

fn persist_failure_artifact(
    runtime: &SimulationRuntimeGuard,
    failure: &serde_json::Value,
) -> Result<(), String> {
    let Ok(directory) = std::env::var("SYNC_STORM_TRACE_DIR") else {
        return Ok(());
    };
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create failure directory {directory}: {error}"))?;
    let identity = runtime
        .artifact_identity()
        .map_err(|error| error.to_string())?;
    let path = std::path::Path::new(&directory).join(format!("failure-{identity}.json"));
    let bytes = serde_json::to_vec_pretty(failure)
        .map_err(|error| format!("serialize failure {}: {error}", path.display()))?;
    std::fs::write(&path, bytes)
        .map_err(|error| format!("write failure {}: {error}", path.display()))
}

pub(super) fn persist_trace_artifact(outcome: &ScenarioOutcome) -> Result<(), String> {
    let Ok(directory) = std::env::var("SYNC_STORM_TRACE_DIR") else {
        return Ok(());
    };
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create trace directory {directory}: {error}"))?;
    let filename = format!(
        "{}-n{}-seed{}-{}-{}.json",
        outcome.topology.name(),
        outcome.count,
        outcome.seed,
        outcome.profile.name(),
        outcome.strategy.name(),
    );
    let path = std::path::Path::new(&directory).join(filename);
    let trace = outcome
        .state
        .trace()
        .canonical_json()
        .map_err(|error| format!("serialize trace {}: {error}", path.display()))?;
    let artifact = serde_json::json!({
        "scenario": {
            "topology": outcome.topology.name(),
            "count": outcome.count,
            "seed": outcome.seed,
            "profile": outcome.profile.name(),
            "strategy": outcome.strategy.name(),
            "replay_command": outcome.replay_command(),
        },
        "trace": serde_json::from_slice::<serde_json::Value>(&trace)
            .map_err(|error| format!("decode canonical trace {}: {error}", path.display()))?,
        "pressure_snapshot": outcome.pressure_snapshot,
        "protection_observations": outcome.protection_observations,
        "capacity_observations": outcome.capacity_observations,
        "overload_witness": outcome.overload_witness,
        "diagnostic": outcome.diagnostic(),
    });
    let bytes = serde_json::to_vec_pretty(&artifact)
        .map_err(|error| format!("serialize artifact {}: {error}", path.display()))?;
    std::fs::write(&path, bytes).map_err(|error| format!("write trace {}: {error}", path.display()))
}

pub(super) fn persist_runtime_artifact(
    label: &str,
    runtime: &SimulationRuntimeGuard,
) -> Result<(), String> {
    let Ok(directory) = std::env::var("SYNC_STORM_TRACE_DIR") else {
        return Ok(());
    };
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create trace directory {directory}: {error}"))?;
    let artifact = serde_json::json!({
        "label": label,
        "scenario": runtime.artifact_context().map_err(|error| error.to_string())?,
        "runtime": runtime_replay_snapshot(runtime)?,
    });
    let identity = runtime
        .artifact_identity()
        .map_err(|error| error.to_string())?;
    let path = std::path::Path::new(&directory).join(format!("runtime-{identity}-{label}.json"));
    let bytes = serde_json::to_vec_pretty(&artifact)
        .map_err(|error| format!("serialize runtime artifact {}: {error}", path.display()))?;
    std::fs::write(&path, bytes)
        .map_err(|error| format!("write runtime artifact {}: {error}", path.display()))
}

pub(super) fn runtime_replay_snapshot(
    runtime: &SimulationRuntimeGuard,
) -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "elapsed_virtual_ms": runtime.elapsed_ms().map_err(|error| error.to_string())?,
        "protection_observations": runtime
            .protection_observations()
            .map_err(|error| error.to_string())?,
        "capacity_observations": runtime
            .capacity_observations()
            .map_err(|error| error.to_string())?,
        "pending_deliveries": runtime
            .pending_deliveries()
            .map_err(|error| error.to_string())?,
    }))
}

pub(super) fn persist_inflight_trace_artifact(
    label: &str,
    runtime: &SimulationRuntimeGuard,
    state: &SimState,
) -> Result<(), String> {
    let Ok(directory) = std::env::var("SYNC_STORM_TRACE_DIR") else {
        return Ok(());
    };
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create trace directory {directory}: {error}"))?;
    let identity = runtime
        .artifact_identity()
        .map_err(|error| error.to_string())?;
    let path = std::path::Path::new(&directory).join(format!("inflight-{identity}-{label}.json"));
    let trace = state
        .trace()
        .canonical_json()
        .map_err(|error| format!("serialize trace {}: {error}", path.display()))?;
    let artifact = serde_json::json!({
        "scenario": runtime.artifact_context().map_err(|error| error.to_string())?,
        "trace": serde_json::from_slice::<serde_json::Value>(&trace)
            .map_err(|error| format!("decode trace {}: {error}", path.display()))?,
        "runtime": runtime_replay_snapshot(runtime)?,
    });
    let bytes = serde_json::to_vec_pretty(&artifact)
        .map_err(|error| format!("serialize trace {}: {error}", path.display()))?;
    std::fs::write(&path, bytes).map_err(|error| format!("write trace {}: {error}", path.display()))
}

pub(super) fn persist_named_trace_artifact(
    label: &str,
    runtime: &SimulationRuntimeGuard,
    state: &SimState,
) -> Result<(), String> {
    let Ok(directory) = std::env::var("SYNC_STORM_TRACE_DIR") else {
        return Ok(());
    };
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create trace directory {directory}: {error}"))?;
    let path = std::path::Path::new(&directory).join(format!("{label}.json"));
    let trace = state
        .trace()
        .canonical_json()
        .map_err(|error| format!("serialize trace {}: {error}", path.display()))?;
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "scenario": runtime.artifact_context().map_err(|error| error.to_string())?,
        "trace": serde_json::from_slice::<serde_json::Value>(&trace)
            .map_err(|error| format!("decode trace {}: {error}", path.display()))?,
    }))
    .map_err(|error| format!("serialize trace {}: {error}", path.display()))?;
    std::fs::write(&path, bytes).map_err(|error| format!("write trace {}: {error}", path.display()))
}
