//! Foreground native TUN gateway supervision.

use std::collections::BTreeSet;
#[cfg(any(test, target_os = "windows"))]
use std::ffi::OsString;
use std::net::IpAddr;
use std::net::Ipv4Addr;
#[cfg(any(test, target_os = "windows"))]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use rings_gateway::bindings::unix::UnixTunnelControl;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rings_gateway::bindings::unix::UnixTunnelOptions;
use rings_gateway::bindings::EstablishedTunnel;
#[cfg(target_os = "windows")]
use rings_gateway::bindings::NativeTunnelControl;
#[cfg(target_os = "windows")]
use rings_gateway::bindings::NativeTunnelOptions;
use rings_gateway::bindings::TunnelControl;
use rings_gateway::bindings::UnderlayPolicy;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use rings_gateway::bindings::UnsupportedTunnelControl;
use rings_gateway::DnsPolicy;
use rings_gateway::ExitAvailability;
use rings_gateway::GatewayControlHandle;
use rings_gateway::GatewayError;
use rings_gateway::GatewayPlan;
use rings_gateway::GatewayRuntime;
use rings_gateway::GatewayStatusHandle;
use rings_transport::connections::UnderlayCandidateAdmission;
use rings_transport::connections::UnderlayCandidateAdmissionError;
use rings_transport::ice_server::IceServer;
use tokio::sync::Mutex;

use super::config::NativeGatewayConfig;
use crate::onion::proxy::OnionProxyConfig;
use crate::onion::tcp::NativeOnionCircuitHandle;
use crate::onion::NativeOnionGatewayConnector;
use crate::prelude::StopSource;
use crate::prelude::StopToken;
use crate::processor::Processor;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use crate::util::expand_home;

#[cfg(target_os = "windows")]
type PlatformTunnelControl = NativeTunnelControl;
#[cfg(any(target_os = "linux", target_os = "macos"))]
type PlatformTunnelControl = UnixTunnelControl;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
type PlatformTunnelControl = UnsupportedTunnelControl;

struct GatewayUnderlayState<C> {
    control: C,
    dynamic_targets: BTreeSet<IpAddr>,
}

/// Serializes every host-route update and retains admitted candidates until teardown.
///
/// Candidate routes are deliberately monotonic during one gateway lease. A periodic topology
/// snapshot can lag behind a just-admitted SDP, so removing a route from such a snapshot could
/// make ICE recurse into the capture route between admission and candidate nomination.
struct GatewayUnderlayGate<C> {
    fixed_targets: BTreeSet<IpAddr>,
    state: Mutex<GatewayUnderlayState<C>>,
}

impl<C> GatewayUnderlayGate<C>
where C: UnderlayPolicy
{
    fn new(control: C, fixed_targets: Vec<IpAddr>) -> Self {
        Self {
            fixed_targets: fixed_targets.into_iter().filter(IpAddr::is_ipv4).collect(),
            state: Mutex::new(GatewayUnderlayState {
                control,
                dynamic_targets: BTreeSet::new(),
            }),
        }
    }

    async fn admit_targets(&self, candidates: &[IpAddr]) -> Result<(), GatewayError> {
        let mut state = self.state.lock().await;
        let mut next_dynamic = state.dynamic_targets.clone();
        next_dynamic.extend(candidates.iter().copied().filter(IpAddr::is_ipv4));
        let desired = self
            .fixed_targets
            .iter()
            .copied()
            .chain(next_dynamic.iter().copied())
            .collect::<Vec<_>>();
        state.control.replace_bypass_targets(&desired).await?;
        state.dynamic_targets = next_dynamic;
        Ok(())
    }
}

impl<C> GatewayUnderlayGate<C>
where C: TunnelControl + UnderlayPolicy
{
    async fn establish(
        &self,
        plan: &GatewayPlan,
    ) -> Result<EstablishedTunnel<C::Device, C::Lease>, GatewayError> {
        self.state.lock().await.control.establish(plan).await
    }

    async fn teardown(&self, lease: C::Lease) -> Result<(), GatewayError> {
        self.state.lock().await.control.teardown(lease).await
    }
}

#[async_trait::async_trait]
impl<C> UnderlayCandidateAdmission for GatewayUnderlayGate<C>
where C: UnderlayPolicy
{
    async fn admit(&self, candidates: &[IpAddr]) -> Result<(), UnderlayCandidateAdmissionError> {
        self.admit_targets(candidates)
            .await
            .map_err(|error| UnderlayCandidateAdmissionError::new(error.to_string()))
    }
}

/// Prepared foreground gateway with a status capability available before platform setup.
pub struct NativeGatewayRunner {
    processor: Arc<Processor>,
    runtime: GatewayRuntime,
    config: NativeGatewayConfig,
    ice_servers: String,
}

impl NativeGatewayRunner {
    /// Build the pure runtime and Onion connector without changing host network state.
    pub fn new(
        processor: Arc<Processor>,
        onion: NativeOnionCircuitHandle,
        config: NativeGatewayConfig,
        ice_servers: String,
    ) -> anyhow::Result<Self> {
        if config.underlay_refresh_secs == 0 {
            anyhow::bail!("gateway underlay_refresh_secs must be greater than zero");
        }
        config.runtime.validate()?;
        validate_ice_dns_compatibility(&config.runtime.plan, &ice_servers)?;
        let proxy = OnionProxyConfig::tcp_connect_service(
            config.onion_service.clone(),
            config.onion_hop_count,
            config.onion_allow_short_paths,
        )?;
        let connector = Arc::new(NativeOnionGatewayConnector::new(
            processor.clone(),
            onion,
            proxy,
        ));
        let runtime = GatewayRuntime::new(config.runtime.clone(), connector, rand::random())?;
        Ok(Self {
            processor,
            runtime,
            config,
            ice_servers,
        })
    }

    /// Return process-independent status for the native HTTP inspection endpoint.
    pub fn status_handle(&self) -> GatewayStatusHandle {
        self.runtime.status_handle()
    }

    /// Establish routes and the packet interface, run until stopped, then reconcile the lease.
    pub async fn run(mut self, stop: StopToken) -> anyhow::Result<()> {
        let control = self.platform_control()?;
        let fixed_targets = self.fixed_underlay_targets().await?;
        let underlay = Arc::new(GatewayUnderlayGate::new(control, fixed_targets));
        let admission: Arc<dyn UnderlayCandidateAdmission> = underlay.clone();
        self.processor
            .swarm
            .set_underlay_candidate_admission(Some(admission))
            .await;
        if let Err(error) = underlay
            .admit_targets(&self.processor.swarm.underlay_remote_ips().await)
            .await
        {
            self.processor
                .swarm
                .set_underlay_candidate_admission(None)
                .await;
            return Err(error.into());
        }
        let EstablishedTunnel {
            mut device,
            lease,
            interface_name,
        } = match underlay.establish(&self.config.runtime.plan).await {
            Ok(established) => established,
            Err(error) => {
                self.processor
                    .swarm
                    .set_underlay_candidate_admission(None)
                    .await;
                return Err(error.into());
            }
        };

        if let Err(error) = self.runtime.activate(interface_name) {
            let cleanup = underlay.teardown(lease).await;
            let cleanup =
                finish_gateway_cleanup(&self.processor, &mut self.runtime, device, cleanup).await;
            return combine_gateway_results(Err(error), Ok(()), cleanup);
        }

        let runtime_done = StopSource::new();
        let updater_failed = StopSource::new();
        let update = refresh_underlay_and_exit(UnderlayRefresh {
            processor: self.processor.clone(),
            underlay: Arc::clone(&underlay),
            gateway: self.runtime.control_handle(),
            onion_service: self.config.onion_service.as_str().to_string(),
            interval: Duration::from_secs(self.config.underlay_refresh_secs),
            stop: stop.clone(),
            runtime_done: runtime_done.token(),
            updater_failed: updater_failed.clone(),
        });
        let updater_failed_token = updater_failed.token();
        let runtime_done_after_run = runtime_done.clone();
        let run = async {
            let result = self.runtime.run(&mut device, || {
                stop.should_stop() || updater_failed_token.should_stop()
            });
            let result = result.await;
            runtime_done_after_run.request_stop();
            result
        };
        let (runtime_result, updater_result) = tokio::join!(run, update);

        let cleanup_result = underlay.teardown(lease).await;
        let cleanup_result =
            finish_gateway_cleanup(&self.processor, &mut self.runtime, device, cleanup_result)
                .await;
        combine_gateway_results(runtime_result, updater_result, cleanup_result)
    }

    #[cfg(target_os = "windows")]
    fn platform_control(&self) -> anyhow::Result<PlatformTunnelControl> {
        let ledger = expand_home(&self.config.route_ledger_path)?;
        let mut options = NativeTunnelOptions::new(ledger);
        if let Some(interface_name) = self.config.interface_name.clone() {
            options = options.with_interface_name(interface_name);
        }
        let environment = std::env::var_os("RINGS_GATEWAY_WINTUN_DLL");
        if let Some(path) =
            select_wintun_dll_path(self.config.wintun_dll_path.as_deref(), environment)?
        {
            options = options.with_wintun_dll(path);
        }
        NativeTunnelControl::new(options).map_err(Into::into)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn platform_control(&self) -> anyhow::Result<PlatformTunnelControl> {
        let socket_path = expand_home(&self.config.unix_helper_socket)?;
        Ok(UnixTunnelControl::new(UnixTunnelOptions::new(socket_path)))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    fn platform_control(&self) -> anyhow::Result<PlatformTunnelControl> {
        Ok(UnsupportedTunnelControl::new())
    }

    async fn fixed_underlay_targets(&self) -> Result<Vec<IpAddr>, GatewayError> {
        let mut targets = self
            .config
            .underlay_bypass_targets
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        targets.extend(resolve_ice_server_ips(&self.ice_servers).await?);
        Ok(targets.into_iter().collect())
    }
}

async fn finish_gateway_cleanup<D: rings_gateway::PacketIo>(
    processor: &Processor,
    runtime: &mut GatewayRuntime,
    device: D,
    cleanup: Result<(), GatewayError>,
) -> Result<(), GatewayError> {
    match cleanup {
        Ok(()) => {
            // Routes are gone before direct candidate admission is restored or the final packet
            // descriptor is closed. This keeps orderly teardown from opening a direct-leak window.
            processor.swarm.set_underlay_candidate_admission(None).await;
            drop(device);
            Ok(())
        }
        Err(error) => {
            // Capture state may still exist. Retain both the admission gate and packet descriptor
            // so a cleanup failure degrades connectivity instead of silently restoring direct IO.
            let reason = format!(
                "gateway route cleanup failed; direct underlay admission and packet-device release \
                 remain blocked until privileged cleanup succeeds: {error}"
            );
            runtime.set_exit_availability(ExitAvailability::Unknown, Some(reason.clone()));
            tracing::error!("{reason}");
            std::mem::forget(device);
            Err(error)
        }
    }
}

fn validate_ice_dns_compatibility(plan: &GatewayPlan, config: &str) -> Result<(), GatewayError> {
    if plan.dns_policy != DnsPolicy::Block || config.trim().is_empty() {
        return Ok(());
    }
    let servers = IceServer::vec_from_str(config).map_err(|error| GatewayError::Platform {
        operation: "validate-ice-dns-policy",
        message: error.to_string(),
    })?;
    for url in servers.into_iter().flat_map(|server| server.urls) {
        let (host, _) = ice_host_port(&url)?;
        if host.parse::<Ipv4Addr>().is_err() {
            return Err(GatewayError::Platform {
                operation: "validate-ice-dns-policy",
                message: format!(
                    "gateway DNS block requires literal IPv4 ICE hosts, but {url:?} uses \
                     {host:?}; configure DNS bypass or a numeric ICE endpoint"
                ),
            });
        }
    }
    Ok(())
}

#[cfg(any(test, target_os = "windows"))]
fn select_wintun_dll_path(
    configured: Option<&str>,
    environment: Option<OsString>,
) -> Result<Option<PathBuf>, crate::error::Error> {
    let selected = configured.map(PathBuf::from).or_else(|| {
        environment
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    });
    selected.map(expand_home).transpose()
}

struct UnderlayRefresh {
    processor: Arc<Processor>,
    underlay: Arc<GatewayUnderlayGate<PlatformTunnelControl>>,
    gateway: GatewayControlHandle,
    onion_service: String,
    interval: Duration,
    stop: StopToken,
    runtime_done: StopToken,
    updater_failed: StopSource,
}

enum RefreshWake {
    Tick,
    Stop,
}

async fn wait_for_refresh(
    ticker: &mut tokio::time::Interval,
    stop: &StopToken,
    runtime_done: &StopToken,
) -> RefreshWake {
    tokio::select! {
        _ = stop.stopped() => RefreshWake::Stop,
        _ = runtime_done.stopped() => RefreshWake::Stop,
        _ = ticker.tick() => RefreshWake::Tick,
    }
}

async fn refresh_underlay_and_exit(refresh: UnderlayRefresh) -> Result<(), GatewayError> {
    let mut ticker = tokio::time::interval(refresh.interval);
    loop {
        if matches!(
            wait_for_refresh(&mut ticker, &refresh.stop, &refresh.runtime_done).await,
            RefreshWake::Stop
        ) {
            return Ok(());
        }
        let targets = refresh.processor.swarm.underlay_remote_ips().await;
        if let Err(error) = refresh.underlay.admit_targets(&targets).await {
            refresh.updater_failed.request_stop();
            return Err(error);
        }

        let (availability, reason) = match refresh
            .processor
            .lookup_onion_exits(&refresh.onion_service, false)
            .await
        {
            Ok(exits) if exits.is_empty() => (
                ExitAvailability::Unavailable,
                Some(format!(
                    "no live Onion TCP exit advertises {}",
                    refresh.onion_service
                )),
            ),
            Ok(_) => (ExitAvailability::Available, None),
            Err(error) => (
                ExitAvailability::Unknown,
                Some(format!("Onion exit discovery failed: {error}")),
            ),
        };
        if refresh
            .gateway
            .set_exit_availability(availability, reason)
            .await
            .is_err()
        {
            if refresh.runtime_done.should_stop() {
                return Ok(());
            }
            refresh.updater_failed.request_stop();
            return Err(GatewayError::Platform {
                operation: "update-gateway-exit-availability",
                message: "gateway runtime control channel closed".to_string(),
            });
        }
    }
}

#[cfg(test)]
fn merged_underlay_targets(fixed: &[IpAddr], dynamic: Vec<IpAddr>) -> Vec<IpAddr> {
    fixed
        .iter()
        .copied()
        .chain(dynamic)
        .filter(IpAddr::is_ipv4)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

async fn resolve_ice_server_ips(config: &str) -> Result<Vec<IpAddr>, GatewayError> {
    if config.trim().is_empty() {
        return Ok(Vec::new());
    }
    let servers = IceServer::vec_from_str(config).map_err(|error| GatewayError::Platform {
        operation: "parse-ice-underlay",
        message: error.to_string(),
    })?;
    let mut addresses = BTreeSet::new();
    for url in servers.into_iter().flat_map(|server| server.urls) {
        let (host, port) = ice_host_port(&url)?;
        let resolved = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|error| GatewayError::Platform {
                operation: "resolve-ice-underlay",
                message: format!("failed to resolve ICE server {host}:{port}: {error}"),
            })?;
        let resolved = resolved
            .map(|address| address.ip())
            .filter(IpAddr::is_ipv4)
            .collect::<BTreeSet<_>>();
        if resolved.is_empty() {
            return Err(GatewayError::Platform {
                operation: "resolve-ice-underlay",
                message: format!("ICE server {host}:{port} has no IPv4 address"),
            });
        }
        addresses.extend(resolved);
    }
    Ok(addresses.into_iter().collect())
}

fn ice_host_port(url: &str) -> Result<(String, u16), GatewayError> {
    let (_, remainder) = url.split_once(':').ok_or_else(|| GatewayError::Platform {
        operation: "parse-ice-underlay",
        message: format!("ICE URL {url:?} has no scheme"),
    })?;
    let authority = remainder
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return Err(GatewayError::Platform {
            operation: "parse-ice-underlay",
            message: format!("ICE URL {url:?} has no host"),
        });
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, suffix) = bracketed
            .split_once(']')
            .ok_or_else(|| GatewayError::Platform {
                operation: "parse-ice-underlay",
                message: format!("ICE URL {url:?} has an invalid IPv6 authority"),
            })?;
        let port = suffix
            .strip_prefix(':')
            .map(str::parse::<u16>)
            .transpose()
            .map_err(|error| GatewayError::Platform {
                operation: "parse-ice-underlay",
                message: format!("ICE URL {url:?} has an invalid port: {error}"),
            })?
            .unwrap_or(3_478);
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => port
            .parse::<u16>()
            .map(|port| (host.to_string(), port))
            .map_err(|error| GatewayError::Platform {
                operation: "parse-ice-underlay",
                message: format!("ICE URL {url:?} has an invalid port: {error}"),
            }),
        _ => Ok((authority.to_string(), 3_478)),
    }
}

fn combine_gateway_results(
    runtime: Result<(), GatewayError>,
    updater: Result<(), GatewayError>,
    cleanup: Result<(), GatewayError>,
) -> anyhow::Result<()> {
    match (runtime, updater, cleanup) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (runtime, updater, cleanup) => {
            let failures = [
                runtime.err().map(|error| format!("data plane: {error}")),
                updater.err().map(|error| format!("underlay: {error}")),
                cleanup.err().map(|error| format!("cleanup: {error}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
            Err(anyhow::anyhow!(failures))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    struct RecordingUnderlayControl {
        updates: Arc<StdMutex<Vec<Vec<IpAddr>>>>,
    }

    #[async_trait::async_trait]
    impl UnderlayPolicy for RecordingUnderlayControl {
        async fn replace_bypass_targets(&mut self, targets: &[IpAddr]) -> Result<(), GatewayError> {
            self.updates
                .lock()
                .expect("recording underlay lock")
                .push(targets.to_vec());
            Ok(())
        }
    }

    #[test]
    fn ice_authority_parser_accepts_transport_normalized_urls() {
        let stun = ice_host_port("stun:stun.example.test:19302/path").expect("valid STUN URL");
        let turn = ice_host_port("turn:turn.example.test").expect("valid TURN URL");
        assert_eq!(stun, ("stun.example.test".to_string(), 19_302));
        assert_eq!(turn, ("turn.example.test".to_string(), 3_478));
    }

    fn gateway_plan(dns_policy: DnsPolicy) -> GatewayPlan {
        GatewayPlan {
            routing_mode: rings_gateway::RoutingMode::Default,
            addresses: vec!["100.64.0.1/30".parse().expect("gateway address")],
            included_routes: vec!["0.0.0.0/0".parse().expect("capture route")],
            excluded_routes: vec!["127.0.0.0/8".parse().expect("excluded route")],
            mtu: rings_gateway::Mtu::try_from(1_280).expect("gateway MTU"),
            dns_policy,
            dns_servers: vec!["1.1.1.1".parse().expect("DNS")],
        }
    }

    #[test]
    fn blocked_dns_requires_literal_ipv4_ice_hosts() {
        let blocked = gateway_plan(DnsPolicy::Block);
        assert!(validate_ice_dns_compatibility(&blocked, "stun://203.0.113.7:3478").is_ok());
        assert!(validate_ice_dns_compatibility(&blocked, "").is_ok());
        let error = validate_ice_dns_compatibility(&blocked, "stun://stun.example.test:3478")
            .expect_err("named ICE host must need DNS after capture");
        assert!(error
            .to_string()
            .contains("requires literal IPv4 ICE hosts"));

        let bypassed = gateway_plan(DnsPolicy::Bypass);
        assert!(validate_ice_dns_compatibility(&bypassed, "stun://stun.example.test:3478").is_ok());
    }

    #[test]
    fn configured_wintun_path_precedes_environment_fallback() {
        let configured = select_wintun_dll_path(
            Some("/configured/wintun.dll"),
            Some(OsString::from("/environment/wintun.dll")),
        )
        .expect("select configured path");
        assert_eq!(configured, Some(PathBuf::from("/configured/wintun.dll")));

        let fallback =
            select_wintun_dll_path(None, Some(OsString::from("/environment/wintun.dll")))
                .expect("select environment path");
        assert_eq!(fallback, Some(PathBuf::from("/environment/wintun.dll")));
    }

    #[test]
    fn merged_underlay_targets_are_sorted_and_unique() {
        let fixed = vec!["203.0.113.1".parse().expect("test IP")];
        let dynamic = vec![
            "198.51.100.2".parse().expect("test IP"),
            "203.0.113.1".parse().expect("test IP"),
            "2001:db8::1".parse().expect("test IPv6 candidate"),
        ];
        assert_eq!(merged_underlay_targets(&fixed, dynamic), vec![
            "198.51.100.2".parse::<IpAddr>().expect("test IP"),
            "203.0.113.1".parse::<IpAddr>().expect("test IP")
        ]);
    }

    #[tokio::test]
    async fn refresh_wait_wakes_immediately_when_stopped() {
        let stop = StopSource::new();
        let runtime_done = StopSource::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(3_600));
        ticker.tick().await;
        let stop_token = stop.token();
        let runtime_done_token = runtime_done.token();
        let wait = wait_for_refresh(&mut ticker, &stop_token, &runtime_done_token);
        stop.request_stop();

        let wake = tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .expect("stop wakes long refresh interval");
        assert!(matches!(wake, RefreshWake::Stop));
    }

    #[tokio::test]
    async fn duplicate_ice_urls_share_one_valid_ipv4_result() {
        let resolved = resolve_ice_server_ips("stun://127.0.0.1:3478;turn://127.0.0.1:3478")
            .await
            .expect("each duplicate ICE URL resolves independently");
        assert_eq!(resolved, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    }

    #[tokio::test]
    async fn candidate_admission_never_removes_an_earlier_candidate() {
        let updates = Arc::new(StdMutex::new(Vec::new()));
        let gate = GatewayUnderlayGate::new(
            RecordingUnderlayControl {
                updates: Arc::clone(&updates),
            },
            vec!["192.0.2.10".parse().expect("fixed IP")],
        );

        gate.admit_targets(&["198.51.100.20".parse().expect("first candidate")])
            .await
            .expect("first admission");
        gate.admit_targets(&["203.0.113.30".parse().expect("second candidate")])
            .await
            .expect("second admission");

        assert_eq!(
            updates.lock().expect("recording underlay lock").as_slice(),
            &[
                vec![
                    "192.0.2.10".parse::<IpAddr>().expect("fixed IP"),
                    "198.51.100.20".parse().expect("first candidate"),
                ],
                vec![
                    "192.0.2.10".parse::<IpAddr>().expect("fixed IP"),
                    "198.51.100.20".parse().expect("first candidate"),
                    "203.0.113.30".parse().expect("second candidate"),
                ],
            ]
        );
    }
}
