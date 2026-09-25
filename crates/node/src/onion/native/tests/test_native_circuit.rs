use std::sync::Arc;

use rings_core::delegation::DelegateeKey;
use rings_core::ecc::SecretKey;
use rings_core::message::MessageSigner;

use super::super::native_onion_runtimes;
use super::super::NativeOnionCircuitHandle;
use super::super::NativeOnionCircuitHandler;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Extensions;
use crate::onion::circuit::OnionCircuitHandler;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionServiceName;
use crate::tests::TEST_NETWORK_ID;

/// One node has one circuit protocol, so a second install is refused instead of splitting state.
#[tokio::test]
async fn test_install_rejects_duplicate_namespace_instead_of_splitting_runtime() -> Result<()> {
    let processor = Arc::new(crate::tests::native::prepare_processor().await);
    let extensions = Extensions::new(processor);
    let _handle = NativeOnionCircuitHandle::install(&extensions)?;

    assert!(matches!(
        NativeOnionCircuitHandle::install(&extensions),
        Err(Error::ExtensionError(_))
    ));
    Ok(())
}

/// The native Σ-algebra registers exactly the configured exit services `Σ_n`, and nothing on a
/// node without an exit configuration.
#[test]
fn test_native_algebra_registers_exactly_the_configured_services() -> Result<()> {
    let session = DelegateeKey::new_with_seckey(&SecretKey::random()).map_err(Error::CoreError)?;
    let policy =
        OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], Vec::new())?;
    let registered = |exit_config: Option<OnionExitOffer>| {
        let (tcp, https) = native_onion_runtimes(session.clone(), TEST_NETWORK_ID, exit_config);
        let handler = NativeOnionCircuitHandler::new(
            tcp,
            https,
            MessageSigner::new(session.clone(), TEST_NETWORK_ID),
        );
        handler.algebra().symbols().cloned().collect::<Vec<_>>()
    };

    assert_eq!(
        registered(Some(OnionExitOffer::new(
            [OnionServiceName::https()],
            policy.clone()
        )?)),
        vec![OnionServiceName::https()]
    );
    assert_eq!(
        registered(Some(OnionExitOffer::new(
            [OnionServiceName::tcp()],
            policy
        )?)),
        vec![OnionServiceName::tcp()]
    );
    assert!(registered(None).is_empty());
    Ok(())
}
