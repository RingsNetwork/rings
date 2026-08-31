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
use rings_gateway::bindings::TeardownFailure;
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
use tokio::sync::oneshot;
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

const TURN_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_UNDERLAY_REFRESH_SECS: u64 = 30;

/// Owns operator authorization and the refreshed routed exclusions for one gateway lease.
///
/// Remote SDP is deliberately excluded from this authority. Enabling this gate switches native
/// WebRTC to relay-only ICE, so only configured TURN servers require host-route exclusions.
struct GatewayUnderlayGate<C> {
    authorized_targets: BTreeSet<IpAddr>,
    state: Mutex<GatewayUnderlayState<C>>,
}

struct GatewayUnderlayState<C> {
    control: C,
    routed_targets: BTreeSet<IpAddr>,
}

impl<C> GatewayUnderlayGate<C>
where C: UnderlayPolicy
{
    fn new(control: C, authorized_targets: Vec<IpAddr>, turn_targets: Vec<IpAddr>) -> Self {
        let authorized_targets = authorized_targets.into_iter().collect::<BTreeSet<_>>();
        let mut routed_targets = authorized_targets.clone();
        routed_targets.extend(turn_targets);
        Self {
            authorized_targets,
            state: Mutex::new(GatewayUnderlayState {
                control,
                routed_targets,
            }),
        }
    }

    async fn install_routed_targets(&self) -> Result<(), GatewayError> {
        let mut state = self.state.lock().await;
        let targets = state.routed_targets.iter().copied().collect::<Vec<_>>();
        state.control.replace_bypass_targets(&targets).await
    }

    async fn refresh_turn_targets(&self, turn_targets: Vec<IpAddr>) -> Result<(), GatewayError> {
        let mut state = self.state.lock().await;
        // Retain prior resolutions for the lease lifetime: an established TURN allocation may
        // keep using an older address after DNS rotates. Remote SDP never reaches this set.
        let mut routed_targets = state.routed_targets.clone();
        routed_targets.extend(turn_targets);
        let targets = routed_targets.iter().copied().collect::<Vec<_>>();
        // Invoke replacement even when DNS is unchanged. Besides making refresh semantics
        // explicit, this is the bounded keepalive for the foreground Unix helper connection.
        state.control.replace_bypass_targets(&targets).await?;
        state.routed_targets = routed_targets;
        Ok(())
    }

    fn authorize_targets(&self, targets: &[IpAddr]) -> Result<(), UnderlayCandidateAdmissionError> {
        if let Some(target) = targets
            .iter()
            .find(|target| !target.is_ipv4() || !self.authorized_targets.contains(target))
        {
            return Err(UnderlayCandidateAdmissionError::new(format!(
                "underlay target {target} is not operator-authorized; add it to gateway \
                 underlay_bypass_targets before capture starts"
            )));
        }
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

    async fn teardown(&self, lease: C::Lease) -> Result<(), TeardownFailure<C::Lease>> {
        self.state.lock().await.control.teardown(lease).await
    }
}

#[async_trait::async_trait]
impl<C> UnderlayCandidateAdmission for GatewayUnderlayGate<C>
where C: UnderlayPolicy
{
    async fn admit(&self, candidates: &[IpAddr]) -> Result<(), UnderlayCandidateAdmissionError> {
        self.authorize_targets(candidates)
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
        validate_underlay_refresh_secs(config.underlay_refresh_secs)?;
        config.runtime.validate()?;
        validate_underlay_bypass_targets(&config.underlay_bypass_targets)?;
        validate_turn_dns_compatibility(&config.runtime.plan, &ice_servers)?;
        validate_gateway_turn_server(&ice_servers)?;
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
    pub async fn run(self, stop: StopToken) -> anyhow::Result<()> {
        self.run_inner(stop, None).await
    }

    /// Run the gateway and publish when relay-only policy and packet capture are both active.
    ///
    /// Foreground supervisors use this barrier before starting the processor listener. Without it,
    /// TURN resolution could yield long enough for a direct native WebRTC connection to enter the
    /// pool before gateway policy is installed.
    pub async fn run_with_startup_barrier(
        self,
        stop: StopToken,
        started: oneshot::Sender<()>,
    ) -> anyhow::Result<()> {
        self.run_inner(stop, Some(started)).await
    }

    async fn run_inner(
        mut self,
        stop: StopToken,
        started: Option<oneshot::Sender<()>>,
    ) -> anyhow::Result<()> {
        let control = self.platform_control()?;
        let turn_targets = resolve_turn_server_ips(&self.ice_servers).await?;
        let underlay = Arc::new(GatewayUnderlayGate::new(
            control,
            self.config.underlay_bypass_targets.clone(),
            turn_targets,
        ));
        underlay.install_routed_targets().await?;
        let admission: Arc<dyn UnderlayCandidateAdmission> = underlay.clone();
        self.processor
            .swarm
            .enable_underlay_candidate_admission(admission)
            .await?;
        let EstablishedTunnel {
            mut device,
            lease,
            interface_name,
        } = match underlay.establish(&self.config.runtime.plan).await {
            Ok(established) => established,
            Err(error) => {
                self.processor
                    .swarm
                    .clear_underlay_candidate_admission()
                    .await;
                return Err(error.into());
            }
        };

        if let Err(error) = self.runtime.activate(interface_name) {
            let cleanup = underlay
                .teardown(lease)
                .await
                .map_err(TeardownFailure::into_error);
            let cleanup =
                finish_gateway_cleanup(&self.processor, &mut self.runtime, device, cleanup).await;
            return combine_gateway_results(Err(error), Ok(()), cleanup);
        }
        if let Some(started) = started {
            let _ = started.send(());
        }

        let runtime_done = StopSource::new();
        let updater_failed = StopSource::new();
        let update = refresh_exit_availability(GatewayRefresh {
            processor: self.processor.clone(),
            gateway: self.runtime.control_handle(),
            onion_service: self.config.onion_service.as_str().to_string(),
            underlay: Arc::clone(&underlay),
            ice_servers: self.ice_servers.clone(),
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

        let cleanup_result = underlay
            .teardown(lease)
            .await
            .map_err(TeardownFailure::into_error);
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
}

fn validate_underlay_bypass_targets(targets: &[IpAddr]) -> Result<(), GatewayError> {
    if let Some(target) = targets.iter().find(|target| target.is_ipv6()) {
        return Err(GatewayError::Platform {
            operation: "validate-gateway-underlay-bypass",
            message: format!("IPv6 underlay target {target} is outside the IPv4 gateway milestone"),
        });
    }
    Ok(())
}

fn validate_underlay_refresh_secs(seconds: u64) -> Result<(), GatewayError> {
    if seconds == 0 || seconds > MAX_UNDERLAY_REFRESH_SECS {
        return Err(GatewayError::Platform {
            operation: "validate-gateway-underlay-refresh",
            message: format!(
                "gateway underlay_refresh_secs must be in 1..={MAX_UNDERLAY_REFRESH_SECS}, got {seconds}"
            ),
        });
    }
    Ok(())
}

async fn finish_gateway_cleanup<D: rings_gateway::PacketIo>(
    processor: &Processor,
    runtime: &mut GatewayRuntime,
    device: D,
    cleanup: Result<(), GatewayError>,
) -> Result<(), GatewayError> {
    match cleanup {
        Ok(()) => {
            // Routes are gone before direct underlay access is restored or the final packet
            // descriptor is closed. This keeps orderly teardown from opening a direct-leak window.
            processor.swarm.clear_underlay_candidate_admission().await;
            drop(device);
            Ok(())
        }
        Err(error) => {
            // Capture state may still exist. Retain both the authorization gate and packet
            // descriptor so a cleanup failure degrades connectivity instead of silently restoring
            // direct IO.
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

fn validate_turn_dns_compatibility(plan: &GatewayPlan, config: &str) -> Result<(), GatewayError> {
    if plan.dns_policy != DnsPolicy::Block || config.trim().is_empty() {
        return Ok(());
    }
    let servers = IceServer::vec_from_str(config).map_err(|error| GatewayError::Platform {
        operation: "validate-turn-dns-policy",
        message: error.to_string(),
    })?;
    for url in servers
        .into_iter()
        .flat_map(|server| server.urls)
        .filter(|url| ice_scheme(url) == Some("turn"))
    {
        let (host, _) = ice_host_port(&url)?;
        if host.parse::<Ipv4Addr>().is_err() {
            return Err(GatewayError::Platform {
                operation: "validate-turn-dns-policy",
                message: format!(
                    "gateway DNS block requires literal IPv4 TURN hosts, but {url:?} uses \
                     {host:?}; configure DNS bypass or a numeric TURN endpoint"
                ),
            });
        }
    }
    Ok(())
}

fn validate_gateway_turn_server(config: &str) -> Result<(), GatewayError> {
    let servers = IceServer::vec_from_str(config).map_err(|error| GatewayError::Platform {
        operation: "validate-gateway-turn-server",
        message: error.to_string(),
    })?;
    let mut turn_count = 0_usize;
    for server in &servers {
        if !server
            .urls
            .iter()
            .any(|url| ice_scheme(url) == Some("turn"))
        {
            continue;
        }
        turn_count += 1;
        if server.username.is_empty() || server.credential.is_empty() {
            return Err(GatewayError::Platform {
                operation: "validate-gateway-turn-server",
                message: "native gateway TURN servers require non-empty username and password credentials"
                    .to_string(),
            });
        }
    }
    if turn_count > 0 {
        return Ok(());
    }
    Err(GatewayError::Platform {
        operation: "validate-gateway-turn-server",
        message:
            "native gateway requires at least one TURN server because WebRTC uses relay-only ICE"
                .to_string(),
    })
}

fn ice_scheme(url: &str) -> Option<&str> {
    url.split_once(':').map(|(scheme, _)| scheme)
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

struct GatewayRefresh {
    processor: Arc<Processor>,
    gateway: GatewayControlHandle,
    onion_service: String,
    underlay: Arc<GatewayUnderlayGate<PlatformTunnelControl>>,
    ice_servers: String,
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

async fn refresh_exit_availability(refresh: GatewayRefresh) -> Result<(), GatewayError> {
    let mut ticker = tokio::time::interval(refresh.interval);
    loop {
        if matches!(
            wait_for_refresh(&mut ticker, &refresh.stop, &refresh.runtime_done).await,
            RefreshWake::Stop
        ) {
            return Ok(());
        }
        let turn_targets = resolve_turn_server_ips(&refresh.ice_servers).await?;
        refresh.underlay.refresh_turn_targets(turn_targets).await?;
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

async fn resolve_turn_server_ips(config: &str) -> Result<Vec<IpAddr>, GatewayError> {
    if config.trim().is_empty() {
        return Ok(Vec::new());
    }
    let servers = IceServer::vec_from_str(config).map_err(|error| GatewayError::Platform {
        operation: "parse-ice-underlay",
        message: error.to_string(),
    })?;
    let mut addresses = BTreeSet::new();
    for url in servers
        .into_iter()
        .flat_map(|server| server.urls)
        .filter(|url| ice_scheme(url) == Some("turn"))
    {
        let (host, port) = ice_host_port(&url)?;
        let resolved = tokio::time::timeout(
            TURN_RESOLUTION_TIMEOUT,
            tokio::net::lookup_host((host.as_str(), port)),
        )
        .await
        .map_err(|_| GatewayError::Platform {
            operation: "resolve-ice-underlay",
            message: format!(
                "timed out after {TURN_RESOLUTION_TIMEOUT:?} resolving TURN server {host}:{port}"
            ),
        })?
        .map_err(|error| GatewayError::Platform {
            operation: "resolve-ice-underlay",
            message: format!("failed to resolve TURN server {host}:{port}: {error}"),
        })?;
        let resolved = resolved
            .map(|address| address.ip())
            .filter(IpAddr::is_ipv4)
            .collect::<BTreeSet<_>>();
        if resolved.is_empty() {
            return Err(GatewayError::Platform {
                operation: "resolve-ice-underlay",
                message: format!("TURN server {host}:{port} has no IPv4 address"),
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
    fn blocked_dns_requires_literal_ipv4_turn_hosts() {
        let blocked = gateway_plan(DnsPolicy::Block);
        assert!(
            validate_turn_dns_compatibility(&blocked, "turn://user:pass@203.0.113.7:3478").is_ok()
        );
        assert!(validate_turn_dns_compatibility(&blocked, "").is_ok());
        let error =
            validate_turn_dns_compatibility(&blocked, "turn://user:pass@turn.example.test:3478")
                .expect_err("named TURN host must need DNS after capture");
        assert!(error
            .to_string()
            .contains("requires literal IPv4 TURN hosts"));

        let bypassed = gateway_plan(DnsPolicy::Bypass);
        assert!(validate_turn_dns_compatibility(
            &bypassed,
            "turn://user:pass@turn.example.test:3478"
        )
        .is_ok());
    }

    #[test]
    fn gateway_requires_turn_for_relay_only_ice() {
        assert!(validate_gateway_turn_server("turn://user:pass@203.0.113.7:3478").is_ok());
        assert!(validate_gateway_turn_server(
            "stun://203.0.113.7:3478;turn://user:pass@203.0.113.8:3478"
        )
        .is_ok());
        let error = validate_gateway_turn_server("turn://203.0.113.7:3478")
            .expect_err("unauthenticated TURN cannot gather a relay candidate");
        assert!(error.to_string().contains("username and password"));
        let error = validate_gateway_turn_server("stun://203.0.113.7:3478")
            .expect_err("STUN alone cannot support relay-only ICE");
        assert!(error.to_string().contains("requires at least one TURN"));
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
    async fn duplicate_turn_urls_share_one_valid_ipv4_result() {
        let resolved = resolve_turn_server_ips(
            "turn://first:pass@127.0.0.1:3478;turn://second:pass@127.0.0.1:3478",
        )
        .await
        .expect("each duplicate TURN URL resolves independently");
        assert_eq!(resolved, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    }

    #[tokio::test]
    async fn underlay_gate_separates_fixed_authority_from_refreshed_turn_routes() {
        let updates = Arc::new(StdMutex::new(Vec::new()));
        let gate = GatewayUnderlayGate::new(
            RecordingUnderlayControl {
                updates: Arc::clone(&updates),
            },
            vec!["192.0.2.10".parse().expect("fixed IP")],
            vec!["198.51.100.10".parse().expect("initial TURN IP")],
        );

        gate.install_routed_targets()
            .await
            .expect("install initial routed targets");
        gate.refresh_turn_targets(vec!["198.51.100.20".parse().expect("rotated TURN IP")])
            .await
            .expect("refresh TURN target");
        gate.admit(&["192.0.2.10".parse().expect("fixed IP")])
            .await
            .expect("authorize fixed target");
        let error = gate
            .admit(&["198.51.100.20".parse().expect("remote target")])
            .await
            .expect_err("unconfigured remote target must not punch a route");

        assert_eq!(
            updates.lock().expect("recording underlay lock").as_slice(),
            &[
                vec![
                    "192.0.2.10".parse::<IpAddr>().expect("fixed IP"),
                    "198.51.100.10".parse::<IpAddr>().expect("initial TURN IP"),
                ],
                vec![
                    "192.0.2.10".parse::<IpAddr>().expect("fixed IP"),
                    "198.51.100.10".parse::<IpAddr>().expect("initial TURN IP"),
                    "198.51.100.20".parse::<IpAddr>().expect("rotated TURN IP"),
                ],
            ]
        );
        assert!(error.to_string().contains("underlay_bypass_targets"));
    }

    #[test]
    fn ipv6_underlay_bypass_is_rejected_instead_of_silently_dropped() {
        let error =
            validate_underlay_bypass_targets(&["2001:db8::7".parse().expect("IPv6 target")])
                .expect_err("IPv6 underlay is unsupported");

        assert!(error.to_string().contains("IPv6 underlay target"));
    }

    #[test]
    fn underlay_refresh_interval_preserves_helper_keepalive_bound() {
        assert!(validate_underlay_refresh_secs(0).is_err());
        assert!(validate_underlay_refresh_secs(1).is_ok());
        assert!(validate_underlay_refresh_secs(MAX_UNDERLAY_REFRESH_SECS).is_ok());
        assert!(validate_underlay_refresh_secs(MAX_UNDERLAY_REFRESH_SECS + 1).is_err());
    }
}
