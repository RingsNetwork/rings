use std::sync::Arc;

use super::super::NativeOnionCircuitHandle;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Extensions;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::runtime::exit_algebra;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionServiceName;

/// One node has one data plane, so a second install is refused instead of splitting its state.
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

/// The native Σ-algebra registers exactly the offered exit services `Σ_n`, and nothing on a
/// node without an offer.
#[test]
fn test_native_algebra_registers_exactly_the_offered_services() -> Result<()> {
    let policy =
        OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], Vec::new())?;
    let registered = |offer: Option<OnionExitOffer>| {
        exit_algebra(
            offer.as_ref(),
            &OnionExitAccounting::default(),
            &OnionLinkSender::default(),
        )
        .symbols()
        .cloned()
        .collect::<Vec<_>>()
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
            policy.clone()
        )?)),
        vec![OnionServiceName::tcp()]
    );
    assert_eq!(
        registered(Some(OnionExitOffer::new(
            [OnionServiceName::tcp(), OnionServiceName::https()],
            policy
        )?)),
        vec![OnionServiceName::https(), OnionServiceName::tcp()]
    );
    assert!(registered(None).is_empty());
    Ok(())
}
