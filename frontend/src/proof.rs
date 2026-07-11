//! Distributed SNARK demo support.

use std::str::FromStr;
use std::time::Duration;

use gloo_timers::future::sleep;
use rings_node::extension::snark::Field;
use rings_node::extension::snark::Input;
use rings_node::extension::snark::ProofResult;
use rings_node::extension::snark::SNARKTaskBuilder;
use rings_node::extension::snark::SupportedPrimeField;
use rings_node::prelude::rings_core::dht::Did;

use crate::node::DemoNode;

/// Public input for the bundled `simple_bn256` circuit.
pub fn sample_input() -> Input {
    vec![(
        "step_in".to_string(),
        vec![
            Field::from_u64(4, SupportedPrimeField::Vesta),
            Field::from_u64(2, SupportedPrimeField::Vesta),
        ],
    )]
    .into()
}

/// Offload a proof task to `prover_did` and wait for a final result.
pub async fn run(
    node: DemoNode,
    prover_did: String,
    r1cs_url: String,
    wasm_url: String,
) -> Result<ProofResult, String> {
    let prover = Did::from_str(prover_did.trim()).map_err(|_| "invalid prover DID".to_string())?;
    let builder = SNARKTaskBuilder::from_remote(r1cs_url, wasm_url, SupportedPrimeField::Vesta)
        .await
        .map_err(|error| format!("load circuit failed: {error}"))?;
    let circuits = builder
        .gen_circuits(sample_input(), vec![], 5)
        .map_err(|error| format!("generate circuits failed: {error}"))?;
    let task_id = node
        .snark
        .gen_and_send_proof_task(node.provider.clone(), circuits, prover)
        .await
        .map_err(|error| format!("send proof task failed: {error}"))?;

    for _attempt in 0..60 {
        sleep(Duration::from_secs(1)).await;
        let result = node
            .snark
            .get_task_result(task_id.clone())
            .map_err(|error| format!("read proof result failed: {error}"))?;
        if result != ProofResult::Pending {
            return Ok(result);
        }
    }
    Ok(ProofResult::Pending)
}

/// Render proof status text.
pub fn result_label(result: ProofResult) -> &'static str {
    match result {
        ProofResult::Verified => "proof verified",
        ProofResult::Invalid => "proof returned but failed verification",
        ProofResult::Pending => "timed out waiting for proof",
    }
}
