//! Shared desktop TUN/Wintun device, route transaction, and stale-state lease.

use std::cmp::Reverse;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Write;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::path::Path;
use std::path::PathBuf;

use ipnet::IpNet;
use serde::Deserialize;
use serde::Serialize;
use tun_rs::DeviceBuilder;
use tun_rs::Layer;

use super::normalize_underlay_targets;
use super::route::Route;
use super::route::RouteManager;
use super::routes::bypass_routes;
use super::routes::capture_routes;
use super::EstablishedTunnel;
use super::TunnelControl;
use super::UnderlayPolicy;
use crate::GatewayError;
use crate::GatewayPlan;
use crate::PacketIo;
use crate::PacketIoError;

/// Platform options for the shared Linux TUN, macOS utun, and Windows Wintun binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeTunnelOptions {
    /// Requested interface name, or `None` for platform allocation.
    pub interface_name: Option<String>,
    /// Durable route journal used for crash reconciliation.
    pub route_ledger_path: PathBuf,
    /// Explicit Windows `wintun.dll` path; ignored on Unix.
    pub wintun_dll_path: Option<PathBuf>,
}

impl NativeTunnelOptions {
    /// Construct options with a caller-owned durable ledger path.
    pub fn new(route_ledger_path: PathBuf) -> Self {
        Self {
            interface_name: None,
            route_ledger_path,
            wintun_dll_path: None,
        }
    }

    /// Request a stable interface name.
    pub fn with_interface_name(mut self, name: String) -> Self {
        self.interface_name = Some(name);
        self
    }

    /// Select an explicit Wintun DLL instead of executable-directory discovery.
    pub fn with_wintun_dll(mut self, path: PathBuf) -> Self {
        self.wintun_dll_path = Some(path);
        self
    }
}

/// Complete-IP-packet adapter around the cross-platform native TUN device.
pub struct NativePacketIo {
    device: tun_rs::AsyncDevice,
}

impl NativePacketIo {
    pub(crate) fn new(device: tun_rs::AsyncDevice) -> Self {
        Self { device }
    }

    /// Transfer the Unix packet descriptor out of its current async runtime registration.
    ///
    /// This is used by the foreground helper's `SCM_RIGHTS` boundary and by namespace-isolated
    /// integration tests. The receiver must reconstruct the adapter with
    /// [`Self::from_owned_fd`] while its target Tokio runtime is active.
    #[cfg(unix)]
    pub fn into_owned_fd(self) -> Result<std::os::fd::OwnedFd, GatewayError> {
        use std::os::fd::FromRawFd;

        let raw_fd = self
            .device
            .into_fd()
            .map_err(|error| GatewayError::platform("export-packet-device", error))?;
        // SAFETY: tun-rs transfers ownership of the valid descriptor returned by `into_fd`.
        // Wrapping it immediately gives the descriptor one RAII owner until SCM_RIGHTS transfer.
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) })
    }

    /// Reconstruct Unix packet IO from one exclusively owned TUN/utun descriptor.
    #[cfg(unix)]
    pub fn from_owned_fd(fd: std::os::fd::OwnedFd) -> Result<Self, GatewayError> {
        use std::os::fd::IntoRawFd;

        let raw_fd = fd.into_raw_fd();
        // SAFETY: `raw_fd` was received as SCM_RIGHTS on the connected helper socket, is owned by
        // this function, and is transferred exactly once into tun-rs. tun-rs installs its own
        // RAII owner before any fallible initialization, so it also closes the descriptor when
        // construction fails.
        unsafe { tun_rs::AsyncDevice::from_fd(raw_fd) }
            .map(Self::new)
            .map_err(|error| GatewayError::platform("import-packet-device", error))
    }
}

#[async_trait::async_trait]
impl PacketIo for NativePacketIo {
    async fn read_packet(&mut self, packet: &mut [u8]) -> Result<usize, PacketIoError> {
        self.device.recv(packet).await.map_err(PacketIoError::Read)
    }

    async fn write_packet(&mut self, packet: &[u8]) -> Result<(), PacketIoError> {
        let written = self
            .device
            .send(packet)
            .await
            .map_err(PacketIoError::Write)?;
        if written != packet.len() {
            return Err(PacketIoError::PartialWrite {
                expected: packet.len(),
                written,
            });
        }
        Ok(())
    }
}

/// Linear route-cleanup capability returned by native tunnel establishment.
pub struct NativeTunnelLease {
    id: u64,
}

struct ActiveTunnel {
    id: u64,
    baseline: Vec<Route>,
    plan: GatewayPlan,
    routes: Vec<Route>,
    bypass: Vec<Route>,
}

/// Desktop tunnel controller with durable, reverse-order route cleanup.
pub struct NativeTunnelControl {
    manager: RouteManager,
    options: NativeTunnelOptions,
    underlay_targets: Vec<IpAddr>,
    active: Option<ActiveTunnel>,
    next_lease_id: u64,
}

impl NativeTunnelControl {
    /// Open the platform route manager without mutating host state.
    pub fn new(options: NativeTunnelOptions) -> Result<Self, GatewayError> {
        let manager =
            RouteManager::new().map_err(|error| GatewayError::platform("route-manager", error))?;
        Ok(Self {
            manager,
            options,
            underlay_targets: Vec::new(),
            active: None,
            next_lease_id: 1,
        })
    }

    /// Delete routes from a previous interrupted lease before a new start.
    pub fn reconcile_stale(&mut self) -> Result<(), GatewayError> {
        if self.active.is_some() {
            return Err(GatewayError::Platform {
                operation: "reconcile-stale",
                message: "cannot reconcile while a native tunnel lease is active".to_string(),
            });
        }
        let Some(mut routes) = read_lease(&self.options.route_ledger_path)? else {
            return Ok(());
        };
        self.cleanup_routes(&mut routes)
    }

    fn establish_inner(
        &mut self,
        plan: &GatewayPlan,
    ) -> Result<EstablishedTunnel<NativePacketIo, NativeTunnelLease>, GatewayError> {
        if self.active.is_some() {
            return Err(GatewayError::Platform {
                operation: "establish",
                message: "native tunnel lease is already active".to_string(),
            });
        }
        let lease_id = self.next_lease_id;
        let next_lease_id = lease_id
            .checked_add(1)
            .ok_or_else(|| GatewayError::Platform {
                operation: "allocate-lease-id",
                message: "native tunnel lease identifier space is exhausted".to_string(),
            })?;
        plan.validate()?;
        validate_underlay_capture_conflicts(plan, &self.underlay_targets)?;
        self.reconcile_stale()?;
        let baseline = self
            .manager
            .list()
            .map_err(|error| GatewayError::platform("list-baseline-routes", error))?;
        let mut installed = Vec::new();
        let mut installed_bypass = Vec::new();

        let result: Result<(tun_rs::AsyncDevice, String), GatewayError> = (|| {
            for network in bypass_routes(plan, &self.underlay_targets) {
                let route = resolve_current_route(&mut self.manager, network)?;
                if !baseline.contains(&route) && !installed.contains(&route) {
                    self.install_route(route.clone(), &mut installed)?;
                    installed_bypass.push(route);
                }
            }

            let (address, prefix) = plan.first_ipv4_address()?;
            let builder = DeviceBuilder::new()
                .layer(Layer::L3)
                .mtu(plan.mtu.get())
                .ipv4(address, prefix, None::<Ipv4Addr>);
            let device = configure_builder(builder, &self.options)
                .build_async()
                .map_err(|error| GatewayError::platform("create-packet-device", error))?;
            for network in plan.addresses.iter().skip(1) {
                let IpNet::V4(network) = network else {
                    continue;
                };
                device
                    .add_address_v4(network.addr(), network.prefix_len())
                    .map_err(|error| GatewayError::platform("add-interface-address", error))?;
            }
            let interface_name = device
                .name()
                .map_err(|error| GatewayError::platform("read-interface-name", error))?;
            let interface_index = device
                .if_index()
                .map_err(|error| GatewayError::platform("read-interface-index", error))?;

            for network in capture_routes(plan) {
                let route = capture_route(network, interface_index);
                self.install_route(route, &mut installed)?;
            }
            write_lease(&self.options.route_ledger_path, &installed)?;
            Ok((device, interface_name))
        })();

        let (device, interface_name) = match result {
            Ok(established) => established,
            Err(error) => {
                let primary = error.to_string();
                let rollback = self.cleanup_routes(&mut installed);
                return match rollback {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(GatewayError::Platform {
                        operation: "establish-rollback",
                        message: format!("{primary}; rollback failed: {cleanup}"),
                    }),
                };
            }
        };

        self.next_lease_id = next_lease_id;
        self.active = Some(ActiveTunnel {
            id: lease_id,
            baseline,
            plan: plan.clone(),
            routes: installed,
            bypass: installed_bypass,
        });
        Ok(EstablishedTunnel {
            device: NativePacketIo::new(device),
            lease: NativeTunnelLease { id: lease_id },
            interface_name,
        })
    }

    fn install_route(
        &mut self,
        route: Route,
        installed: &mut Vec<Route>,
    ) -> Result<(), GatewayError> {
        let ledger = &self.options.route_ledger_path;
        let manager = &mut self.manager;
        journal_then_add_route(
            route,
            installed,
            |routes| write_lease(ledger, routes),
            |route| {
                manager
                    .add(route)
                    .map_err(|error| GatewayError::platform("add-route", error))
            },
        )
    }

    fn cleanup_routes(&mut self, routes: &mut Vec<Route>) -> Result<(), GatewayError> {
        while let Some(route) = routes.pop() {
            if let Err(error) = self.manager.delete(&route) {
                if !route_is_absent(&error) {
                    routes.push(route);
                    write_lease(&self.options.route_ledger_path, routes)?;
                    return Err(GatewayError::platform("delete-route", error));
                }
            }
            write_lease(&self.options.route_ledger_path, routes)?;
        }
        remove_ledger(&self.options.route_ledger_path)
    }

    fn replace_active_bypass(
        &mut self,
        mut active: ActiveTunnel,
        targets: &[IpAddr],
    ) -> (ActiveTunnel, Result<(), GatewayError>) {
        let desired = match bypass_routes(&active.plan, targets)
            .into_iter()
            .map(|network| inherit_baseline_route(&active.baseline, network))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(routes) => routes,
            Err(error) => return (active, Err(error)),
        };

        for route in &desired {
            if active.baseline.contains(route) || active.routes.contains(route) {
                continue;
            }
            if let Err(error) = self.install_route(route.clone(), &mut active.routes) {
                return (active, Err(error));
            }
            active.bypass.push(route.clone());
        }

        let obsolete = active
            .bypass
            .iter()
            .filter(|route| !desired.contains(route))
            .cloned()
            .collect::<Vec<_>>();
        for route in obsolete {
            if let Err(error) = self.manager.delete(&route) {
                if !route_is_absent(&error) {
                    return (
                        active,
                        Err(GatewayError::platform("delete-bypass-route", error)),
                    );
                }
            }
            active.bypass.retain(|candidate| candidate != &route);
            active.routes.retain(|candidate| candidate != &route);
            if let Err(error) = write_lease(&self.options.route_ledger_path, &active.routes) {
                return (active, Err(error));
            }
        }
        (active, Ok(()))
    }
}

fn journal_then_add_route<J, A>(
    route: Route,
    installed: &mut Vec<Route>,
    journal: J,
    add: A,
) -> Result<(), GatewayError>
where
    J: FnOnce(&[Route]) -> Result<(), GatewayError>,
    A: FnOnce(&Route) -> Result<(), GatewayError>,
{
    // Write-ahead invariant: every route that may exist in the OS is already present in the
    // durable ledger. A crash after this write but before `add` leaves only a harmless cleanup
    // intent; a crash after `add` leaves enough information for the next start to remove it.
    installed.push(route);
    journal(installed)?;
    let route = installed.last().ok_or_else(|| GatewayError::Platform {
        operation: "add-route",
        message: "write-ahead route ledger lost its pending route".to_string(),
    })?;
    add(route)
}

#[async_trait::async_trait]
impl TunnelControl for NativeTunnelControl {
    type Device = NativePacketIo;
    type Lease = NativeTunnelLease;

    async fn establish(
        &mut self,
        plan: &GatewayPlan,
    ) -> Result<EstablishedTunnel<Self::Device, Self::Lease>, GatewayError> {
        self.establish_inner(plan)
    }

    async fn teardown(&mut self, lease: Self::Lease) -> Result<(), GatewayError> {
        let Some(mut active) = self.active.take() else {
            return Err(GatewayError::Platform {
                operation: "teardown",
                message: "no native tunnel lease is active".to_string(),
            });
        };
        if active.id != lease.id {
            let active_id = active.id;
            self.active = Some(active);
            return Err(GatewayError::Platform {
                operation: "teardown",
                message: format!(
                    "lease {} does not own active native tunnel lease {active_id}",
                    lease.id
                ),
            });
        }
        let result = self.cleanup_routes(&mut active.routes);
        if result.is_err() {
            self.active = Some(active);
        }
        result
    }
}

#[async_trait::async_trait]
impl UnderlayPolicy for NativeTunnelControl {
    async fn replace_bypass_targets(&mut self, targets: &[IpAddr]) -> Result<(), GatewayError> {
        let normalized = normalize_underlay_targets(targets)?;
        if let Some(active) = self.active.as_ref() {
            validate_underlay_capture_conflicts(&active.plan, &normalized)?;
        }
        if let Some(active) = self.active.take() {
            let (active, result) = self.replace_active_bypass(active, &normalized);
            self.active = Some(active);
            result?;
        }
        self.underlay_targets = normalized;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct RouteLedger {
    routes: Vec<RouteRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RouteRecord {
    destination: IpAddr,
    prefix: u8,
    gateway: Option<IpAddr>,
    interface_name: Option<String>,
    interface_index: Option<u32>,
    metric: Option<u32>,
    table: Option<u8>,
    preferred_source: Option<IpAddr>,
    source: Option<IpAddr>,
    source_prefix: Option<u8>,
    luid: Option<u64>,
    interface_scope: bool,
}

impl RouteRecord {
    fn from_route(route: &Route) -> Self {
        Self {
            destination: route.destination(),
            prefix: route.prefix(),
            gateway: route.gateway(),
            interface_name: route.if_name().cloned(),
            interface_index: route.if_index(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            metric: route.metric(),
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            metric: None,
            #[cfg(target_os = "linux")]
            table: Some(route.table()),
            #[cfg(not(target_os = "linux"))]
            table: None,
            #[cfg(target_os = "linux")]
            preferred_source: route.pref_source(),
            #[cfg(not(target_os = "linux"))]
            preferred_source: None,
            #[cfg(target_os = "linux")]
            source: route.source(),
            #[cfg(not(target_os = "linux"))]
            source: None,
            #[cfg(target_os = "linux")]
            source_prefix: Some(route.source_prefix()),
            #[cfg(not(target_os = "linux"))]
            source_prefix: None,
            #[cfg(target_os = "windows")]
            luid: route.luid(),
            #[cfg(not(target_os = "windows"))]
            luid: None,
            #[cfg(target_os = "macos")]
            interface_scope: route.if_scope(),
            #[cfg(not(target_os = "macos"))]
            interface_scope: false,
        }
    }

    fn into_route(self) -> Route {
        let mut route = Route::new(self.destination, self.prefix);
        if let Some(gateway) = self.gateway {
            route = route.with_gateway(gateway);
        }
        if let Some(name) = self.interface_name {
            route = route.with_if_name(name);
        }
        if let Some(index) = self.interface_index {
            route = route.with_if_index(index);
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        if let Some(metric) = self.metric {
            route = route.with_metric(metric);
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(table) = self.table {
                route = route.with_table(table);
            }
            if let Some(source) = self.source {
                route = route.with_source(source, self.source_prefix.unwrap_or_default());
            }
            if let Some(source) = self.preferred_source {
                route = route.with_pref_source(source);
            }
        }
        #[cfg(target_os = "windows")]
        if let Some(luid) = self.luid {
            route = route.with_luid(luid);
        }
        #[cfg(target_os = "macos")]
        {
            route = route.with_if_scope(self.interface_scope);
        }
        route
    }
}

fn validate_underlay_capture_conflicts(
    plan: &GatewayPlan,
    targets: &[IpAddr],
) -> Result<(), GatewayError> {
    let capture = capture_routes(plan);
    let conflict = targets.iter().find(|target| {
        target.is_ipv4()
            && capture
                .iter()
                .any(|route| route.prefix_len() == 32 && route.contains(*target))
    });
    match conflict {
        Some(target) => Err(GatewayError::Platform {
            operation: "validate-underlay-route",
            message: format!(
                "underlay target {target} is also an exact capture route; use a broader capture \
                 network or remove the target from capture"
            ),
        }),
        None => Ok(()),
    }
}

fn inherit_baseline_route(baseline: &[Route], network: IpNet) -> Result<Route, GatewayError> {
    let target = network.addr();
    let original = baseline
        .iter()
        .filter(|route| route.contains(&target))
        .max_by_key(|route| baseline_route_rank(route))
        .ok_or_else(|| GatewayError::Platform {
            operation: "resolve-bypass-route",
            message: format!("no baseline route reaches {target}"),
        })?;
    inherit_route(original, network)
}

fn resolve_current_route(
    manager: &mut RouteManager,
    network: IpNet,
) -> Result<Route, GatewayError> {
    let target = network.addr();
    let original = manager
        .find_route(&target)
        .map_err(|error| GatewayError::platform("resolve-current-bypass-route", error))?
        .ok_or_else(|| GatewayError::Platform {
            operation: "resolve-current-bypass-route",
            message: format!("no current route reaches {target}"),
        })?;
    inherit_route(&original, network)
}

fn inherit_route(original: &Route, network: IpNet) -> Result<Route, GatewayError> {
    let target = network.addr();
    let mut route = Route::new(network.network(), network.prefix_len());
    if let Some(gateway) = original.gateway() {
        route = route.with_gateway(gateway);
    }
    if let Some(name) = original.if_name() {
        route = route.with_if_name(name.clone());
    }
    if let Some(index) = original.if_index() {
        route = route.with_if_index(index);
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if let Some(metric) = original.metric() {
        route = route.with_metric(metric);
    }
    #[cfg(target_os = "linux")]
    {
        route = route.with_table(original.table());
        if let Some(source) = original.source() {
            route = route.with_source(source, original.source_prefix());
        }
        if let Some(source) = original.pref_source() {
            route = route.with_pref_source(source);
        }
    }
    #[cfg(target_os = "windows")]
    if let Some(luid) = original.luid() {
        route = route.with_luid(luid);
    }
    #[cfg(target_os = "macos")]
    {
        route = route.with_if_scope(original.if_scope());
    }
    if route.gateway().is_none() && route.if_index().is_none() && route.if_name().is_none() {
        return Err(GatewayError::Platform {
            operation: "resolve-bypass-route",
            message: format!("baseline route for {target} has no gateway or interface"),
        });
    }
    Ok(route)
}

fn baseline_route_rank(route: &Route) -> (u8, Reverse<u32>) {
    (route.prefix(), Reverse(baseline_route_metric(route)))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn baseline_route_metric(route: &Route) -> u32 {
    route.metric().unwrap_or(u32::MAX)
}

#[cfg(target_os = "macos")]
fn baseline_route_metric(_route: &Route) -> u32 {
    0
}

fn capture_route(network: IpNet, interface_index: u32) -> Route {
    let route = Route::new(network.network(), network.prefix_len()).with_if_index(interface_index);
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let route = route.with_metric(0);
    route
}

#[cfg(target_os = "linux")]
fn configure_builder(builder: DeviceBuilder, options: &NativeTunnelOptions) -> DeviceBuilder {
    super::unix::linux::configure(builder, options)
}

#[cfg(target_os = "macos")]
fn configure_builder(builder: DeviceBuilder, options: &NativeTunnelOptions) -> DeviceBuilder {
    super::unix::macos::configure(builder, options)
}

#[cfg(target_os = "windows")]
fn configure_builder(builder: DeviceBuilder, options: &NativeTunnelOptions) -> DeviceBuilder {
    super::windows::configure(builder, options)
}

fn read_lease(path: &Path) -> Result<Option<Vec<Route>>, GatewayError> {
    let routes = read_ledger_file(path)?.unwrap_or_default();
    #[cfg(target_os = "windows")]
    let routes = {
        let mut routes = routes;
        if let Some(backup) = read_ledger_file(&ledger_backup_path(path))? {
            for route in backup {
                if !routes.contains(&route) {
                    routes.push(route);
                }
            }
        }
        routes
    };
    if routes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(routes))
    }
}

fn read_ledger_file(path: &Path) -> Result<Option<Vec<Route>>, GatewayError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(GatewayError::platform("read-route-ledger", error)),
    };
    let ledger =
        serde_json::from_slice::<RouteLedger>(&bytes).map_err(|error| GatewayError::Platform {
            operation: "decode-route-ledger",
            message: error.to_string(),
        })?;
    Ok(Some(
        ledger
            .routes
            .into_iter()
            .map(RouteRecord::into_route)
            .collect(),
    ))
}

fn write_lease(path: &Path, routes: &[Route]) -> Result<(), GatewayError> {
    if routes.is_empty() {
        return remove_ledger(path);
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| GatewayError::platform("create-ledger-directory", error))?;
    }
    let ledger = RouteLedger {
        routes: routes.iter().map(RouteRecord::from_route).collect(),
    };
    let bytes = serde_json::to_vec(&ledger).map_err(|error| GatewayError::Platform {
        operation: "encode-route-ledger",
        message: error.to_string(),
    })?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = File::create(&temporary)
        .map_err(|error| GatewayError::platform("create-route-ledger", error))?;
    file.write_all(&bytes)
        .map_err(|error| GatewayError::platform("write-route-ledger", error))?;
    file.sync_all()
        .map_err(|error| GatewayError::platform("sync-route-ledger", error))?;
    #[cfg(target_os = "windows")]
    return commit_windows_ledger(path, &temporary);
    #[cfg(not(target_os = "windows"))]
    std::fs::rename(&temporary, path)
        .map_err(|error| GatewayError::platform("commit-route-ledger", error))
}

fn remove_ledger(path: &Path) -> Result<(), GatewayError> {
    let primary = remove_ledger_file(path);
    #[cfg(target_os = "windows")]
    let backup = remove_ledger_file(&ledger_backup_path(path));
    #[cfg(not(target_os = "windows"))]
    let backup = Ok(());
    match (primary, backup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(backup)) => Err(GatewayError::Platform {
            operation: "remove-route-ledger",
            message: format!("{primary}; backup cleanup failed: {backup}"),
        }),
    }
}

fn remove_ledger_file(path: &Path) -> Result<(), GatewayError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(GatewayError::platform("remove-route-ledger", error)),
    }
}

#[cfg(target_os = "windows")]
fn ledger_backup_path(path: &Path) -> PathBuf {
    path.with_extension("bak")
}

#[cfg(target_os = "windows")]
fn commit_windows_ledger(path: &Path, temporary: &Path) -> Result<(), GatewayError> {
    let backup = ledger_backup_path(path);
    if !path.exists() && backup.exists() {
        std::fs::rename(&backup, path)
            .map_err(|error| GatewayError::platform("restore-route-ledger-backup", error))?;
    }
    remove_ledger_file(&backup)?;
    let had_primary = path.exists();
    if had_primary {
        std::fs::rename(path, &backup)
            .map_err(|error| GatewayError::platform("backup-route-ledger", error))?;
    }
    if let Err(error) = std::fs::rename(temporary, path) {
        let primary = GatewayError::platform("commit-route-ledger", error);
        if had_primary {
            return match std::fs::rename(&backup, path) {
                Ok(()) => Err(primary),
                Err(restore) => Err(GatewayError::Platform {
                    operation: "restore-route-ledger-backup",
                    message: format!("{primary}; backup restore failed: {restore}"),
                }),
            };
        }
        return Err(primary);
    }
    remove_ledger_file(&backup)
}

fn route_is_absent(error: &std::io::Error) -> bool {
    if error.kind() == ErrorKind::NotFound {
        return true;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // A route bound to a TUN/utun interface may disappear when the last packet descriptor is
        // closed. Linux reports ENXIO or ENODEV when the recorded interface index is already gone;
        // BSD route sockets report ESRCH for an already-absent route. All three satisfy the
        // cleanup postcondition: the journalled route no longer exists in the kernel.
        matches!(
            error.raw_os_error(),
            Some(nix::libc::ENXIO | nix::libc::ENODEV | nix::libc::ESRCH)
        )
    }
    #[cfg(target_os = "windows")]
    {
        false
    }
}

#[cfg(test)]
mod tests;
