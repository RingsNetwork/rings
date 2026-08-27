use std::sync::Arc;

use rings_core::ecc::SecretKey;
use rings_core::session::SessionSk;
use rings_core::storage::MemStorage;
use rings_node::error::Error;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_node::registration::OnlineNodeRegistration;

use crate::Field;
use crate::Input;
use crate::Result;
use crate::SNARKBehaviour;
use crate::SNARKTaskBuilder;
use crate::SupportedPrimeField;
use crate::CAPABILITY;

async fn prepare_processor() -> Result<Processor> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).map_err(Error::CoreError)?;
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        3,
    );
    let storage = Box::new(MemStorage::new());
    let processor = ProcessorBuilder::from_config(&config)?
        .storage(storage)
        .build()?;
    Ok(processor)
}

fn fixture_path(name: &str) -> String {
    format!(
        "{}/../../crates/snark/src/tests/native/circoms/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[tokio::test]
async fn test_registered_snark_extension_declares_capability() -> Result<()> {
    let processor = prepare_processor().await?;
    let descriptor = processor.publish_online_node_descriptor().await?;
    assert_eq!(
        descriptor.capabilities,
        OnlineNodeRegistration::default_capabilities()
    );
    assert!(!descriptor
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY));

    let provider = Provider::from_processor(Arc::new(processor.clone()));
    SNARKBehaviour::default().register(&provider)?;
    let descriptor = processor.publish_online_node_descriptor().await?;

    assert!(descriptor
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY));
    Ok(())
}

#[tokio::test]
async fn test_gen_proof_and_verify() -> Result<()> {
    let wasm = fixture_path("simple_bn256.wasm");
    let r1cs = fixture_path("simple_bn256.r1cs");
    let snark_task_builder =
        SNARKTaskBuilder::from_local(r1cs, wasm, SupportedPrimeField::Vesta).await?;
    let input: Input = vec![("step_in".to_string(), vec![
        Field::from_u64(4, SupportedPrimeField::Vesta),
        Field::from_u64(2, SupportedPrimeField::Vesta),
    ])]
    .into();
    let circuits = snark_task_builder.gen_circuits(input, vec![], 5)?;
    assert_eq!(circuits.len(), 5);
    let task = SNARKBehaviour::gen_proof_task(circuits)?;
    let proof = SNARKBehaviour::handle_snark_proof_task(&task)?;
    let ret = SNARKBehaviour::handle_snark_verify_task(&proof, &task)?;
    assert!(ret);
    Ok(())
}
