//! Foreground native TUN gateway supervision.

#[cfg(any(test, target_os = "windows"))]
use std::ffi::OsString;
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
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use rings_gateway::bindings::UnsupportedTunnelControl;
use rings_gateway::ExitObservation;
use rings_gateway::GatewayControlHandle;
use rings_gateway::GatewayError;
use rings_gateway::GatewayRuntime;
use rings_gateway::GatewayStatusHandle;
use tokio::sync::oneshot;

use super::config::NativeGatewayConfig;
use crate::onion::native::NativeOnionCircuitHandle;
use crate::onion::NativeOnionGatewayConnector;
use crate::onion::OnionServiceName;
use crate::prelude::StopSource;
use crate::prelude::StopToken;
use crate::processor::Processor;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use crate::util::expand_home;

/// Platform-specific tunnel control backend used by [`NativeGatewayRunner`].
#[cfg(target_os = "windows")]
type PlatformTunnelControl = NativeTunnelControl;
/// Platform-specific tunnel control backend used by [`NativeGatewayRunner`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
type PlatformTunnelControl = UnixTunnelControl;
/// Platform-specific tunnel control backend used by [`NativeGatewayRunner`].
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
type PlatformTunnelControl = UnsupportedTunnelControl;

/// Maximum allowed interval in seconds for refreshing gateway exit availability.
const MAX_STATUS_REFRESH_SECS: u64 = 30;

/// Prepared foreground gateway with a status capability available before platform setup.
pub struct NativeGatewayRunner {
    /// Node processor used to query live Onion exit availability.
    processor: Arc<Processor>,
    /// Underlying gateway packet-forwarding runtime.
    runtime: GatewayRuntime,
    /// Validated native gateway configuration.
    config: NativeGatewayConfig,
}

impl NativeGatewayRunner {
    /// Build the pure runtime and Onion connector without changing host network state.
    pub fn new(
        processor: Arc<Processor>,
        onion: NativeOnionCircuitHandle,
        config: NativeGatewayConfig,
    ) -> anyhow::Result<Self> {
        validate_status_refresh_secs(config.status_refresh_secs)?;
        let connector = Arc::new(NativeOnionGatewayConnector::new(processor.clone(), onion));
        let runtime = GatewayRuntime::new(config.runtime.clone(), connector, rand::random())?;
        Ok(Self {
            processor,
            runtime,
            config,
        })
    }

    /// Return process-independent status for the native HTTP inspection endpoint.
    pub fn status_handle(&self) -> GatewayStatusHandle {
        self.runtime.status_handle()
    }

    /// Run the gateway and publish when its explicitly selected packet ingress is active.
    pub async fn run_with_startup_barrier(
        self,
        stop: StopToken,
        started: oneshot::Sender<()>,
    ) -> anyhow::Result<()> {
        self.run_inner(stop, Some(started)).await
    }

    /// Establishes the TUN device, runs the packet loop and exit-availability updater, and tears down.
    async fn run_inner(
        mut self,
        stop: StopToken,
        started: Option<oneshot::Sender<()>>,
    ) -> anyhow::Result<()> {
        let mut control = self.platform_control()?;
        let EstablishedTunnel {
            mut device,
            lease,
            interface_name,
        } = control.establish(&self.config.runtime.plan).await?;

        if let Err(error) = self.runtime.activate(interface_name) {
            let cleanup = control
                .teardown(lease)
                .await
                .map_err(TeardownFailure::into_error);
            let cleanup = finish_gateway_cleanup(&mut self.runtime, device, cleanup);
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
            interval: Duration::from_secs(self.config.status_refresh_secs),
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

        let cleanup_result = control
            .teardown(lease)
            .await
            .map_err(TeardownFailure::into_error);
        let cleanup_result = finish_gateway_cleanup(&mut self.runtime, device, cleanup_result);
        combine_gateway_results(runtime_result, updater_result, cleanup_result)
    }

    /// Constructs the Windows tunnel controller from gateway configuration and environment.
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

    /// Constructs the Unix privileged-helper tunnel controller.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn platform_control(&self) -> anyhow::Result<PlatformTunnelControl> {
        let socket_path = expand_home(&self.config.unix_helper_socket)?;
        Ok(UnixTunnelControl::new(UnixTunnelOptions::new(socket_path)))
    }

    /// Constructs a stub tunnel controller for unsupported platforms.
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    fn platform_control(&self) -> anyhow::Result<PlatformTunnelControl> {
        Ok(UnsupportedTunnelControl::new())
    }
}

/// Validates that `seconds` is within `1..=MAX_STATUS_REFRESH_SECS`.
fn validate_status_refresh_secs(seconds: u64) -> Result<(), GatewayError> {
    if seconds == 0 || seconds > MAX_STATUS_REFRESH_SECS {
        return Err(GatewayError::Platform {
            operation: "validate-gateway-status-refresh",
            message: format!(
                "gateway status_refresh_secs must be in 1..={MAX_STATUS_REFRESH_SECS}, got {seconds}"
            ),
        });
    }
    Ok(())
}

/// Finalizes tunnel device cleanup, intentionally leaking `device` if route teardown failed.
fn finish_gateway_cleanup<D: rings_gateway::PacketIo>(
    runtime: &mut GatewayRuntime,
    device: D,
    cleanup: Result<(), GatewayError>,
) -> Result<(), GatewayError> {
    match cleanup {
        Ok(()) => {
            drop(device);
            Ok(())
        }
        Err(error) => {
            // An explicitly selected route may remain installed. Retaining the packet descriptor
            // keeps that selected traffic fail-closed until privileged cleanup succeeds; unrelated
            // host traffic remains outside the gateway's route authority.
            let reason = format!(
                "gateway route cleanup failed; the packet device remains open for selected routes \
                 until privileged cleanup succeeds: {error}"
            );
            runtime.set_exit_availability(ExitObservation::Unknown(reason.clone()));
            tracing::error!("{reason}");
            std::mem::forget(device);
            Err(error)
        }
    }
}

/// Resolves the optional Wintun DLL path, preferring config over the environment variable.
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

/// Parameters for the periodic Onion exit-availability refresh task.
struct GatewayRefresh {
    /// Node processor queried for advertised Onion TCP exits.
    processor: Arc<Processor>,
    /// Control handle used to push exit observations into the gateway runtime.
    gateway: GatewayControlHandle,
    /// Polling interval between exit-availability checks.
    interval: Duration,
    /// External stop token cancelling the gateway.
    stop: StopToken,
    /// Stop token fired once the packet data-plane loop exits.
    runtime_done: StopToken,
    /// Stop source signalled if the status updater loses its control channel.
    updater_failed: StopSource,
}

/// Event that woke [`wait_for_refresh`].
enum RefreshWake {
    /// Refresh interval elapsed and exit availability should be queried.
    Tick,
    /// Shutdown or runtime completion was requested.
    Stop,
}

/// Waits for either the next refresh tick or a stop signal.
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

/// Periodically queries live Onion TCP exits and updates gateway availability state.
async fn refresh_exit_availability(refresh: GatewayRefresh) -> Result<(), GatewayError> {
    let mut ticker = tokio::time::interval(refresh.interval);
    loop {
        if matches!(
            wait_for_refresh(&mut ticker, &refresh.stop, &refresh.runtime_done).await,
            RefreshWake::Stop
        ) {
            return Ok(());
        }
        let exit = match refresh
            .processor
            .lookup_onion_exits(OnionServiceName::tcp().as_str(), false)
            .await
        {
            Ok(exits) if exits.is_empty() => ExitObservation::Unavailable(format!(
                "no live Onion TCP exit advertises {}",
                OnionServiceName::tcp().as_str()
            )),
            Ok(_) => ExitObservation::Available,
            Err(error) => ExitObservation::Unknown(format!("Onion exit discovery failed: {error}")),
        };
        if refresh.gateway.set_exit_availability(exit).await.is_err() {
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

/// Combines data-plane, status-updater, and teardown results without hiding any failure.
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
                updater
                    .err()
                    .map(|error| format!("status updater: {error}")),
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
    use super::*;

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
    fn status_refresh_interval_is_bounded() {
        assert!(validate_status_refresh_secs(0).is_err());
        assert!(validate_status_refresh_secs(1).is_ok());
        assert!(validate_status_refresh_secs(MAX_STATUS_REFRESH_SECS).is_ok());
        assert!(validate_status_refresh_secs(MAX_STATUS_REFRESH_SECS + 1).is_err());
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

    #[test]
    fn result_aggregation_preserves_all_failure_boundaries() {
        let error = |operation| GatewayError::Platform {
            operation,
            message: "failed".to_string(),
        };
        let combined = combine_gateway_results(
            Err(error("runtime")),
            Err(error("status")),
            Err(error("cleanup")),
        )
        .expect_err("three failures must remain visible")
        .to_string();

        assert!(combined.contains("data plane"));
        assert!(combined.contains("status updater"));
        assert!(combined.contains("cleanup"));
    }
}
