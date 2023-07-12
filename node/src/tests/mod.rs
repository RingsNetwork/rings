#[cfg(feature = "browser")]
pub mod wasm;

use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

pub async fn prepare_processor(key: SecretKey) -> Processor {}
