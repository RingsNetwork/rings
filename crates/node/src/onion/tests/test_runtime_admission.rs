//! The onion runtime's registration delegates its namespace's admission (#888, #895 R4-M3).

use std::sync::Arc;

use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::SecretKey;

use crate::error::Result;
use crate::extension::ext::Envelope;
use crate::extension::ext::Extensions;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::runtime::OnionRuntime;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

/// The extensions of a fresh processor that advertises nothing.
fn extensions() -> Result<Extensions> {
    let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let config = ProcessorConfig::new(1, String::new(), session, 1);
    let processor = ProcessorBuilder::from_config(&config)?
        .advertise_presence(false)
        .build()?;
    Ok(Extensions::new(Arc::new(processor)))
}

/// Installing the onion runtime makes core skip the per-origin message count for onion cells,
/// which the L9 link charge admits instead, and for nothing else.
#[tokio::test]
async fn test_the_onion_runtime_delegates_only_its_namespace() -> Result<()> {
    let extensions = extensions()?;
    let cell = Envelope::new(ONION_CIRCUIT_NAMESPACE, Bytes::from_static(b"cell")).encode()?;
    let other = Envelope::new("echo", Bytes::from_static(b"x")).encode()?;
    assert!(!extensions.delegates_admission(&cell));

    let _runtime = OnionRuntime::install(&extensions)?;
    assert!(extensions.delegates_admission(&cell));
    assert!(!extensions.delegates_admission(&other));
    Ok(())
}
