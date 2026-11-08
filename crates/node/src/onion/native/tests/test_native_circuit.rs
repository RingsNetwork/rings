use std::sync::Arc;

use rings_core::delegation::DelegateeKey;
use rings_core::utils::get_epoch_ms;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Extensions;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::native::NativeOnionCircuitHandle;
use crate::onion::runtime::exit_algebra;
use crate::onion::runtime::OnionRuntime;
use crate::onion::tests::fixture::fixture_route_hop;
use crate::onion::tests::fixture::fixture_secret;
use crate::onion::tests::fixture::session_loop;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionLoopClass;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;
use crate::tests::exit_descriptor::ExitDescriptorFixture;

/// One node has one data plane, so a second install is refused instead of splitting its state.
#[tokio::test]
async fn test_install_rejects_duplicate_namespace_instead_of_splitting_runtime() -> Result<()> {
    let processor = Arc::new(crate::tests::native::prepare_processor().await);
    let extensions = Extensions::new(processor);
    let _runtime = OnionRuntime::install(&extensions)?;

    assert!(matches!(
        OnionRuntime::install(&extensions),
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

/// `open_tcp_stream_with_class` refuses a route selected for another symbol before opening a
/// session, regardless of the requested loop class `b`.
#[tokio::test]
async fn test_open_tcp_stream_with_class_rejects_non_tcp_route() -> Result<()> {
    let processor = Arc::new(crate::tests::native::prepare_processor().await);
    let extensions = Extensions::new(Arc::clone(&processor));
    let handle = NativeOnionCircuitHandle::new(OnionRuntime::install(&extensions)?);
    let policy =
        OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], Vec::new())?;
    let descriptor = ExitDescriptorFixture {
        node_type: OnlineNodeType::Native,
        ..ExitDescriptorFixture::live(
            &processor,
            OnionServiceName::https(),
            policy,
            get_epoch_ms(),
        )
    }
    .sign()?;
    let hops = [1_u8, 2, 3, 4].map(|seed| {
        let key =
            DelegateeKey::new_with_seckey(&fixture_secret(seed)).expect("fixture delegatee key");
        fixture_route_hop(&key, OnionProcessEpoch::new([seed; 16]))
    });
    let route = OnionRoute::new(OnionServiceName::https(), session_loop(hops), descriptor);
    let target = OnionProxyTarget::parse_authority("example.com:443")?;

    let result = handle
        .open_tcp_stream_with_class(route, target, OnionLoopClass::KiB64)
        .await;

    assert!(matches!(
        result,
        Err(Error::OnionRouteError(
            OnionRouteError::PayloadServiceMismatch {
                ref payload_service,
                ref route_service,
            }
        )) if payload_service == "tcp" && route_service == "https"
    ));
    Ok(())
}
