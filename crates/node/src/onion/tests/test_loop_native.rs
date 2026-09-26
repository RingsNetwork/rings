//! End-to-end loops over real links (#843): `relay^k ⋙ tcp` and `relay^k ⋙ https`, with `k = 4`
//! relay positions around one symbol hop.

use std::time::Instant;

use bytes::Bytes;

use super::loop_network::open_policy;
use super::loop_network::EchoWorld;
use super::loop_network::LoopNetwork;
use super::loop_network::SourceWorld;
use super::loop_network::EVENT_BOUND;
use crate::error::Error;
use crate::error::Result;
use crate::onion::circuit::OnionAlgebra;
use crate::onion::https::OnionHttpsCall;
use crate::onion::https::OnionHttpsClient;
use crate::onion::https::OnionHttpsEgress;
use crate::onion::https::OnionHttpsRequest;
use crate::onion::https::OnionHttpsResponse;
use crate::onion::https::OnionHttpsWorld;
use crate::onion::proxy::OnionProxyProtocol;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::session::client::OnionCreditWindow;
use crate::onion::session::dial::OnionSessionRequest;
use crate::onion::session::dial::OnionStreamEvent;
use crate::onion::session::dial::OnionStreamReceiver;
use crate::onion::session::serve::OnionExitSessions;
use crate::onion::session::serve::OnionWorld;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;

/// The fixture target.
fn target() -> OnionProxyTarget {
    OnionProxyTarget::parse_authority("example.com:443").expect("authority")
}

/// A network whose `h` interprets `symbol` over `world` under `policy`.
async fn network<W: OnionWorld>(
    symbol: OnionServiceName,
    world: W,
    policy: OnionExitPolicy,
) -> Result<LoopNetwork> {
    LoopNetwork::new(move |accounting, link_sender| {
        OnionAlgebra::default().register(
            symbol,
            OnionExitSessions::new(world, policy, accounting.clone(), link_sender.clone()),
        )
    })
    .await
}

/// The session request of `symbol` to [`target`] over `network`'s route.
fn request(network: &LoopNetwork, symbol: OnionServiceName) -> Result<OnionSessionRequest> {
    Ok(OnionSessionRequest {
        route: network.route(symbol.clone())?,
        symbol,
        target: target(),
        class: OnionLoopClass::DEFAULT,
        window: OnionCreditWindow::DEFAULT,
    })
}

/// Collect the world-to-client stream until `fin`, failing on a failed session.
async fn collect(receiver: &mut OnionStreamReceiver) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        match receiver.next().await {
            Some(OnionStreamEvent::Data(data)) => bytes.extend_from_slice(&data),
            Some(OnionStreamEvent::Fin) => return Ok(bytes),
            Some(OnionStreamEvent::Failed) | None => {
                return Err(Error::OnionRouteError(OnionRouteError::SessionFailed))
            }
        }
    }
}

/// `relay^4 ⋙ tcp` over an echo: the stream returns byte for byte, across several frames, and
/// the client's `fin` ends the echo's stream.
#[tokio::test]
#[ignore = "five real-WebRTC processors: until #887 each hop-send blocks ~290 ms, so a loaded suite run exceeds the 30 s session bounds (#883); run with --ignored"]
async fn test_tcp_loop_echoes_a_multi_frame_stream() -> Result<()> {
    let network = network(OnionServiceName::tcp(), EchoWorld, open_policy()).await?;
    let stream = network
        .client
        .runtime
        .open(request(&network, OnionServiceName::tcp())?)
        .await?;
    let (mut sender, mut receiver) = stream.split();
    let sent = (0..40_000_u32)
        .map(|index| u8::try_from(index % 251).expect("small"))
        .collect::<Vec<_>>();

    sender.send(Bytes::from(sent.clone())).await?;
    sender.fin().await?;
    let echoed = tokio::time::timeout(EVENT_BOUND, collect(&mut receiver))
        .await
        .map_err(|_| Error::OnionProxyRequestTimedOut)??;

    assert_eq!(echoed, sent);
    Ok(())
}

/// A target the exit policy denies is refused at the open: `fin` before any data, and no
/// reason on the wire (#843 Q5).
#[tokio::test]
#[ignore = "five real-WebRTC processors: until #887 each hop-send blocks ~290 ms, so a loaded suite run exceeds the 30 s session bounds (#883); run with --ignored"]
async fn test_a_denied_target_is_refused_at_the_open() -> Result<()> {
    let deny = OnionExitPolicy::from_target_strings(vec!["other.example:443".to_string()], vec![])?;
    let network = network(OnionServiceName::tcp(), EchoWorld, deny).await?;

    let opened = network
        .client
        .runtime
        .open(request(&network, OnionServiceName::tcp())?)
        .await;

    assert!(matches!(
        opened,
        Err(Error::OnionRouteError(OnionRouteError::ExitRefused))
    ));
    Ok(())
}

/// The test egress: a response naming the target and echoing the request.
fn respond(target: &OnionProxyTarget, request: &OnionHttpsRequest) -> OnionHttpsResponse {
    OnionHttpsResponse {
        status: 200,
        headers: vec![("x-target".to_string(), target.authority())],
        body: [
            request.method.as_bytes(),
            b" ",
            request.path.as_bytes(),
            b" ",
        ]
        .concat()
        .into_iter()
        .chain(request.body.iter().copied())
        .collect(),
    }
}

/// `relay^4 ⋙ https` through the fetch world over a test egress: the request's authority is
/// the session's target, and the response returns whole.
#[tokio::test]
#[ignore = "five real-WebRTC processors: until #887 each hop-send blocks ~290 ms, so a loaded suite run exceeds the 30 s session bounds (#883); run with --ignored"]
async fn test_https_loop_fetches_through_the_session_target() -> Result<()> {
    let network = LoopNetwork::new(|accounting, link_sender| {
        OnionAlgebra::default().register(
            OnionServiceName::https(),
            OnionExitSessions::new(
                OnionHttpsWorld::with_egress(
                    open_policy(),
                    accounting.clone(),
                    OnionHttpsEgress::Test(respond),
                ),
                open_policy(),
                accounting.clone(),
                link_sender.clone(),
            ),
        )
    })
    .await?;
    let (target, call) = OnionHttpsCall::from_url(
        "https://example.com/search?q=loop",
        crate::onion::https::OnionHttpsClientRequest {
            method: "POST".to_string(),
            body: vec![0x42; 20_000],
            ..Default::default()
        },
    )?;
    let route = OnionProxyRoute {
        protocol: OnionProxyProtocol::HttpsProxy,
        target,
        route: network.route(OnionServiceName::https())?,
    };

    let response = tokio::time::timeout(
        EVENT_BOUND,
        OnionHttpsClient::new(network.client.runtime.clone()).request(&route, call),
    )
    .await
    .map_err(|_| Error::OnionProxyRequestTimedOut)??;

    assert_eq!(response.status, 200);
    assert_eq!(response.headers, vec![(
        "x-target".to_string(),
        "example.com:443".to_string()
    )]);
    assert_eq!(
        response.body,
        [
            b"POST /search?q=loop ".as_slice(),
            [0x42; 20_000].as_slice()
        ]
        .concat()
    );
    Ok(())
}

/// The throughput bench (#843): a `tcp` download of 8 MiB at `b = 16 KiB` over loopback links,
/// at least 1 MB/s. Run with `--ignored`; it reports the rate it measured.
#[tokio::test]
#[ignore = "benchmark"]
async fn bench_tcp_download_rate() -> Result<()> {
    const DOWNLOAD: usize = 8 << 20;
    let network = network(
        OnionServiceName::tcp(),
        SourceWorld(DOWNLOAD),
        open_policy(),
    )
    .await?;
    let stream = network
        .client
        .runtime
        .open(request(&network, OnionServiceName::tcp())?)
        .await?;
    let (_sender, mut receiver) = stream.split();

    let started = Instant::now();
    let received = collect(&mut receiver).await?;
    let seconds = started.elapsed().as_secs_f64();
    let rate = f64::from(u32::try_from(received.len()).expect("small")) / seconds;
    println!(
        "tcp download: {} bytes in {seconds:.2} s = {rate:.0} B/s",
        received.len()
    );

    assert_eq!(received.len(), DOWNLOAD);
    assert!(rate >= 1_000_000.0, "{rate:.0} B/s is under 1 MB/s");
    Ok(())
}
