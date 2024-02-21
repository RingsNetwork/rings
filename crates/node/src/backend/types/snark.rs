//! Backend message types for SNARK
//! ==============================
use rings_snark::prelude::nova::provider::hyperkzg::Bn256EngineKZG;
use rings_snark::prelude::nova::provider::GrumpkinEngine;
use rings_snark::prelude::nova::provider::PallasEngine;
use rings_snark::prelude::nova::provider::VestaEngine;
use serde::Deserialize;
use serde::Serialize;

use crate::backend::snark::SNARKGenerator;
use crate::backend::BackendMessage;

/// Message for snark task
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SNARKTaskMessage {
    /// uuid of task
    pub task_id: uuid::Uuid,
    /// task details
    #[serde(
        serialize_with = "crate::util::serialize_gzip",
        deserialize_with = "crate::util::deserialize_gzip"
    )]
    pub task: SNARKTask,
}

#[cfg(feature = "snark")]
/// Message types for snark task, including proof and verify
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SNARKTask {
    /// Proof task
    SNARKProof(SNARKProofTask),
    /// Verify task
    SNARKVerify(SNARKVerifyTask),
}

/// Message type of snark proof
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SNARKProofTask {
    /// SNARK with curve pallas and vesta
    PallasVasta(SNARKGenerator<PallasEngine, VestaEngine>),
    /// SNARK with curve vesta and pallas
    VastaPallas(SNARKGenerator<VestaEngine, PallasEngine>),
    /// SNARK with curve bn256 whth KZG multi linear commitment and grumpkin
    Bn256KZGGrumpkin(SNARKGenerator<Bn256EngineKZG, GrumpkinEngine>),
}

/// Message type of snark proof
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SNARKVerifyTask {
    /// SNARK with curve pallas and vesta
    PallasVasta(String),
    /// SNARK with curve vesta and pallas
    VastaPallas(String),
    /// SNARK with curve bn256 whth KZG multi linear commitment and grumpkin
    Bn256KZGGrumpkin(String),
}

impl From<SNARKTaskMessage> for BackendMessage {
    fn from(val: SNARKTaskMessage) -> Self {
        BackendMessage::SNARKTaskMessage(val)
    }
}
