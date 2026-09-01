//! Stable replay and failure artifacts for sync-storm scenarios.

use super::*;

fn artifact_directory() -> Result<Option<std::path::PathBuf>, String> {
    let Ok(directory) = std::env::var("SYNC_STORM_TRACE_DIR") else {
        return Ok(None);
    };
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create artifact directory {directory}: {error}"))?;
    Ok(Some(std::path::PathBuf::from(directory)))
}

fn artifact_path(
    filename: impl AsRef<std::path::Path>,
) -> Result<Option<std::path::PathBuf>, String> {
    Ok(artifact_directory()?.map(|directory| directory.join(filename)))
}

fn identified_artifact_path(
    runtime: &SimulationRuntimeGuard,
    prefix: &str,
    suffix: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    let Some(directory) = artifact_directory()? else {
        return Ok(None);
    };
    let identity = runtime
        .artifact_identity()
        .map_err(|error| error.to_string())?;
    Ok(Some(
        directory.join(format!("{prefix}-{identity}{suffix}.json")),
    ))
}

fn persist_json(path: &std::path::Path, artifact: &impl serde::Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(artifact)
        .map_err(|error| format!("serialize artifact {}: {error}", path.display()))?;
    std::fs::write(path, bytes)
        .map_err(|error| format!("write artifact {}: {error}", path.display()))
}

fn trace_json(state: &SimState) -> Result<serde_json::Value, String> {
    let bytes = state
        .trace()
        .canonical_json()
        .map_err(|error| format!("serialize canonical trace: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("decode canonical trace: {error}"))
}

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
    let Some(path) = identified_artifact_path(runtime, "failure", "")? else {
        return Ok(());
    };
    persist_json(&path, failure)
}

pub(super) fn persist_trace_artifact(outcome: &ScenarioOutcome) -> Result<(), String> {
    let filename = format!(
        "{}-n{}-seed{}-{}-{}.json",
        outcome.topology.name(),
        outcome.count,
        outcome.seed,
        outcome.profile.name(),
        outcome.strategy.name(),
    );
    let Some(path) = artifact_path(filename)? else {
        return Ok(());
    };
    let artifact = serde_json::json!({
        "scenario": {
            "topology": outcome.topology.name(),
            "count": outcome.count,
            "seed": outcome.seed,
            "profile": outcome.profile.name(),
            "strategy": outcome.strategy.name(),
            "replay_command": outcome.replay_command(),
        },
        "trace": trace_json(&outcome.state)?,
        "pressure_snapshot": outcome.pressure_snapshot,
        "protection_observations": outcome.protection_observations,
        "capacity_observations": outcome.capacity_observations,
        "overload_witness": outcome.overload_witness,
        "diagnostic": outcome.diagnostic(),
    });
    persist_json(&path, &artifact)
}

pub(super) fn persist_runtime_artifact(
    label: &str,
    runtime: &SimulationRuntimeGuard,
) -> Result<(), String> {
    let Some(path) = identified_artifact_path(runtime, "runtime", &format!("-{label}"))? else {
        return Ok(());
    };
    let artifact = serde_json::json!({
        "label": label,
        "scenario": runtime.artifact_context().map_err(|error| error.to_string())?,
        "runtime": runtime_replay_snapshot(runtime)?,
    });
    persist_json(&path, &artifact)
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
    let Some(path) = identified_artifact_path(runtime, "inflight", &format!("-{label}"))? else {
        return Ok(());
    };
    let artifact = serde_json::json!({
        "scenario": runtime.artifact_context().map_err(|error| error.to_string())?,
        "trace": trace_json(state)?,
        "runtime": runtime_replay_snapshot(runtime)?,
    });
    persist_json(&path, &artifact)
}

pub(super) fn persist_named_trace_artifact(
    label: &str,
    runtime: &SimulationRuntimeGuard,
    state: &SimState,
) -> Result<(), String> {
    let Some(path) = artifact_path(format!("{label}.json"))? else {
        return Ok(());
    };
    let artifact = serde_json::json!({
        "scenario": runtime.artifact_context().map_err(|error| error.to_string())?,
        "trace": trace_json(state)?,
    });
    persist_json(&path, &artifact)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_json_preserves_the_canonical_trace_shape() -> Result<(), String> {
        assert_eq!(
            trace_json(&SimState::default())?,
            serde_json::json!({"events": []})
        );
        Ok(())
    }
}
