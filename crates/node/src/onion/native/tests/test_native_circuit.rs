use std::sync::Arc;

use super::super::NativeOnionCircuitHandle;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Extensions;

/// One node has one circuit protocol, so a second install is refused instead of splitting state.
#[tokio::test]
async fn test_install_rejects_duplicate_namespace_instead_of_splitting_runtime() -> Result<()> {
    let processor = Arc::new(crate::tests::native::prepare_processor().await);
    let delegatee_key = processor.delegatee_key().clone();
    let network_id = processor.swarm.network_id();
    let extensions = Extensions::new(processor);
    let _handle = NativeOnionCircuitHandle::install(
        &extensions,
        delegatee_key.clone(),
        network_id,
        false,
        None,
    )?;

    assert!(matches!(
        NativeOnionCircuitHandle::install(&extensions, delegatee_key, network_id, false, None),
        Err(Error::ExtensionError(_))
    ));
    Ok(())
}
