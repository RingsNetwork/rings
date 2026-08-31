//! Foreground privileged helper for Unix TUN/utun and route configuration.

use std::fs::FileType;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use nix::unistd::chown;
use nix::unistd::dup;
use nix::unistd::Uid;

use super::config::UnixConfigRequest;
use super::config::UnixConfigResponse;
use super::transport::is_request_timeout;
use super::transport::read_request;
use super::transport::send_response;
use crate::bindings::NativeTunnelControl;
use crate::bindings::NativeTunnelLease;
use crate::bindings::NativeTunnelOptions;
use crate::bindings::TunnelControl;
use crate::bindings::UnderlayPolicy;
use crate::GatewayError;
use crate::GatewayPlan;

const HELPER_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Configuration for one foreground Unix helper lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnixHelperOptions {
    /// Control socket path. Existing non-socket paths are never removed.
    pub socket_path: PathBuf,
    /// Optional numeric owner assigned after binding, for a root helper serving one user.
    pub socket_owner: Option<u32>,
    /// Native interface and durable route-ledger options.
    pub tunnel: NativeTunnelOptions,
}

impl UnixHelperOptions {
    /// Construct helper options with default owner preservation.
    pub fn new(socket_path: PathBuf, tunnel: NativeTunnelOptions) -> Self {
        Self {
            socket_path,
            socket_owner: None,
            tunnel,
        }
    }

    /// Assign the bound mode-0600 socket to one numeric user identifier.
    pub fn with_socket_owner(mut self, uid: u32) -> Self {
        self.socket_owner = Some(uid);
        self
    }
}

/// Serve one gateway lifecycle, retaining fail-closed state across client reconnections.
///
/// The helper remains a foreground process. It does not install or invoke a service manager. If
/// the node connection disappears while a lease is active, the helper keeps its packet descriptor
/// and capture routes so traffic blackholes until the same plan reconnects or explicitly tears
/// down the lease.
pub fn serve(mut options: UnixHelperOptions) -> Result<(), GatewayError> {
    let helper_uid = Uid::effective().as_raw();
    let authorized_uid = options.socket_owner.unwrap_or(helper_uid);
    options.socket_path = secure_child_path(
        &options.socket_path,
        helper_uid,
        "validate-unix-helper-path",
    )?;
    options.tunnel.route_ledger_path = secure_child_path(
        &options.tunnel.route_ledger_path,
        helper_uid,
        "validate-route-ledger-path",
    )?;
    remove_stale_socket(&options.socket_path)?;
    let listener = UnixListener::bind(&options.socket_path)
        .map_err(|error| GatewayError::platform("bind-unix-helper", error))?;
    let mut guard = SocketGuard::new(options.socket_path.clone());
    let configured = configure_socket(&options);
    let served = configured.and_then(|()| serve_listener(listener, options.tunnel, authorized_uid));
    let cleaned = guard.cleanup();
    combine_results(served, cleaned, "serve-unix-helper")
}

fn serve_listener(
    listener: UnixListener,
    tunnel_options: NativeTunnelOptions,
    authorized_uid: u32,
) -> Result<(), GatewayError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| GatewayError::platform("build-unix-helper-runtime", error))?;
    let mut control = NativeTunnelControl::new(tunnel_options)?;
    let mut active = None;

    loop {
        let mut stream = match accept_authorized(&listener, authorized_uid) {
            Ok(stream) => stream,
            Err(error) if active.is_some() => {
                eprintln!(
                    "gateway helper accept failed while capture remains fail-closed: {error}"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Err(error) => return Err(error),
        };
        match serve_connection(&runtime, &mut control, &mut active, &mut stream) {
            Ok(HelperConnectionExit::Disconnected) => {}
            Ok(HelperConnectionExit::TimedOut) => {
                eprintln!(
                    "gateway helper client request timed out; any active capture remains fail-closed"
                );
            }
            Ok(HelperConnectionExit::TornDown) => return Ok(()),
            Err(error) if active.is_some() => {
                eprintln!(
                    "gateway helper client failed while capture remains fail-closed: {error}"
                );
            }
            Err(error) => return Err(error),
        }
    }
}

struct ActiveHelperTunnel {
    lease_id: String,
    lease: Option<NativeTunnelLease>,
    interface_name: String,
    plan: GatewayPlan,
    descriptor: OwnedFd,
}

enum HelperConnectionExit {
    Disconnected,
    TimedOut,
    TornDown,
}

fn serve_connection(
    runtime: &tokio::runtime::Runtime,
    control: &mut NativeTunnelControl,
    active: &mut Option<ActiveHelperTunnel>,
    stream: &mut UnixStream,
) -> Result<HelperConnectionExit, GatewayError> {
    loop {
        let request = match read_request(stream) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(HelperConnectionExit::Disconnected),
            Err(error) if is_request_timeout(&error) => {
                return Ok(HelperConnectionExit::TimedOut);
            }
            Err(error) => return Err(error),
        };
        match request {
            UnixConfigRequest::Establish {
                plan,
                underlay_targets,
            } => establish_or_resume(runtime, control, active, stream, plan, underlay_targets)?,
            UnixConfigRequest::ReplaceBypass {
                lease_id,
                underlay_targets,
            } => {
                if active.as_ref().map(|tunnel| tunnel.lease_id.as_str()) != Some(lease_id.as_str())
                {
                    send_failure(
                        stream,
                        "replace-underlay-bypass",
                        "request does not own the active helper lease".to_string(),
                    )?;
                    continue;
                }
                match runtime.block_on(control.replace_bypass_targets(&underlay_targets)) {
                    Ok(()) => send_response(stream, &UnixConfigResponse::Updated, None)?,
                    Err(error) => {
                        send_failure(stream, "replace-underlay-bypass", error.to_string())?
                    }
                }
            }
            UnixConfigRequest::Teardown { lease_id } => {
                if active.as_ref().map(|tunnel| tunnel.lease_id.as_str()) != Some(lease_id.as_str())
                {
                    send_failure(
                        stream,
                        "teardown",
                        "request does not own the active helper lease".to_string(),
                    )?;
                    continue;
                }
                let Some(lease) = active.as_mut().and_then(|tunnel| tunnel.lease.take()) else {
                    send_failure(
                        stream,
                        "teardown",
                        "active helper lease cannot be retried".to_string(),
                    )?;
                    continue;
                };
                match runtime.block_on(control.teardown(lease)) {
                    Ok(()) => {
                        let response = send_response(stream, &UnixConfigResponse::TornDown, None);
                        *active = None;
                        response?;
                        return Ok(HelperConnectionExit::TornDown);
                    }
                    Err(failure) => {
                        let (lease, error) = failure.into_parts();
                        if let Some(tunnel) = active.as_mut() {
                            tunnel.lease = Some(lease);
                        }
                        send_failure(stream, "teardown", error.to_string())?;
                        // Keep serving this authenticated connection. The client receives the
                        // failure together with its linear lease and may retry without restarting
                        // either side of the privilege boundary.
                    }
                }
            }
        }
    }
}

fn establish_or_resume(
    runtime: &tokio::runtime::Runtime,
    control: &mut NativeTunnelControl,
    active: &mut Option<ActiveHelperTunnel>,
    stream: &mut UnixStream,
    plan: GatewayPlan,
    underlay_targets: Vec<std::net::IpAddr>,
) -> Result<(), GatewayError> {
    if let Some(tunnel) = active.as_ref() {
        if tunnel.plan != plan {
            return send_failure(
                stream,
                "establish",
                "an active fail-closed tunnel can only resume with the same plan".to_string(),
            );
        }
        if tunnel.lease.is_none() {
            return send_failure(
                stream,
                "establish",
                "the active tunnel is retained after a failed teardown and cannot resume"
                    .to_string(),
            );
        }
        match runtime.block_on(control.replace_bypass_targets(&underlay_targets)) {
            Ok(()) => return send_established(stream, tunnel),
            Err(error) => {
                return send_failure(stream, "replace-underlay-bypass", error.to_string());
            }
        }
    }

    if let Err(error) = runtime.block_on(control.replace_bypass_targets(&underlay_targets)) {
        return send_failure(stream, "replace-underlay-bypass", error.to_string());
    }
    let established = match runtime.block_on(control.establish(&plan)) {
        Ok(established) => established,
        Err(error) => return send_failure(stream, "establish", error.to_string()),
    };
    let crate::bindings::EstablishedTunnel {
        device,
        lease,
        interface_name,
    } = established;
    let descriptor = match device.into_owned_fd() {
        Ok(descriptor) => descriptor,
        Err(error) => {
            let cleanup = runtime
                .block_on(control.teardown(lease))
                .map_err(crate::bindings::TeardownFailure::into_error);
            return combine_results(Err(error), cleanup, "export-unix-helper-device");
        }
    };
    *active = Some(ActiveHelperTunnel {
        lease_id: format!("{}-1", std::process::id()),
        lease: Some(lease),
        interface_name,
        plan,
        descriptor,
    });
    let tunnel = active.as_ref().ok_or_else(|| {
        GatewayError::platform("establish", "helper lost the active tunnel before response")
    })?;
    send_established(stream, tunnel)
}

fn send_established(
    stream: &mut UnixStream,
    active: &ActiveHelperTunnel,
) -> Result<(), GatewayError> {
    let transfer = dup(&active.descriptor)
        .map_err(|error| GatewayError::platform("duplicate-packet-device", error))?;
    let response = UnixConfigResponse::Established {
        lease_id: active.lease_id.clone(),
        interface_name: active.interface_name.clone(),
    };
    send_response(stream, &response, Some(transfer.as_raw_fd()))
}

fn accept_authorized(
    listener: &UnixListener,
    authorized_uid: u32,
) -> Result<UnixStream, GatewayError> {
    loop {
        let (stream, _) = listener
            .accept()
            .map_err(|error| GatewayError::platform("accept-unix-helper", error))?;
        if peer_uid(&stream)? == authorized_uid {
            stream
                .set_read_timeout(Some(HELPER_REQUEST_TIMEOUT))
                .and_then(|()| stream.set_write_timeout(Some(HELPER_REQUEST_TIMEOUT)))
                .map_err(|error| GatewayError::platform("configure-unix-helper-peer", error))?;
            return Ok(stream);
        }
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Result<u32, GatewayError> {
    nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
        .map(|credentials| credentials.uid())
        .map_err(|error| GatewayError::platform("authenticate-unix-helper-peer", error))
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> Result<u32, GatewayError> {
    nix::unistd::getpeereid(stream)
        .map(|(uid, _)| uid.as_raw())
        .map_err(|error| GatewayError::platform("authenticate-unix-helper-peer", error))
}

fn configure_socket(options: &UnixHelperOptions) -> Result<(), GatewayError> {
    std::fs::set_permissions(&options.socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| GatewayError::platform("chmod-unix-helper", error))?;
    if let Some(uid) = options.socket_owner {
        chown(&options.socket_path, Some(Uid::from_raw(uid)), None)
            .map_err(|error| GatewayError::platform("chown-unix-helper", error))?;
    }
    Ok(())
}

fn secure_child_path(
    path: &Path,
    helper_uid: u32,
    operation: &'static str,
) -> Result<PathBuf, GatewayError> {
    let file_name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| GatewayError::Platform {
            operation,
            message: format!("path {} has no file name", path.display()),
        })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| GatewayError::Platform {
            operation,
            message: format!("path {} has no parent directory", path.display()),
        })?;
    let canonical_parent =
        std::fs::canonicalize(parent).map_err(|error| GatewayError::platform(operation, error))?;
    for (depth, ancestor) in canonical_parent.ancestors().enumerate() {
        let metadata = std::fs::symlink_metadata(ancestor)
            .map_err(|error| GatewayError::platform(operation, error))?;
        if !metadata.is_dir() {
            return Err(GatewayError::Platform {
                operation,
                message: format!("ancestor {} is not a directory", ancestor.display()),
            });
        }
        let owner = metadata.uid();
        if (depth == 0 && owner != helper_uid) || (owner != helper_uid && owner != 0) {
            return Err(GatewayError::Platform {
                operation,
                message: format!(
                    "ancestor {} is owned by UID {owner}; direct parent must belong to helper UID \
                     {helper_uid}, and higher ancestors to helper UID {helper_uid} or root",
                    ancestor.display()
                ),
            });
        }
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(GatewayError::Platform {
                operation,
                message: format!(
                    "ancestor {} has unsafe group/other write bits {:o}",
                    ancestor.display(),
                    mode & 0o777
                ),
            });
        }
    }
    Ok(canonical_parent.join(file_name))
}

fn remove_stale_socket(path: &Path) -> Result<(), GatewayError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(GatewayError::platform("inspect-unix-helper-socket", error)),
    };
    ensure_socket(path, metadata.file_type())?;
    std::fs::remove_file(path)
        .map_err(|error| GatewayError::platform("remove-stale-unix-helper", error))
}

fn ensure_socket(path: &Path, file_type: FileType) -> Result<(), GatewayError> {
    if file_type.is_socket() {
        Ok(())
    } else {
        Err(GatewayError::Platform {
            operation: "remove-stale-unix-helper",
            message: format!("refusing to remove non-socket path {}", path.display()),
        })
    }
}

fn send_failure(
    stream: &mut UnixStream,
    operation: &str,
    message: String,
) -> Result<(), GatewayError> {
    send_response(
        stream,
        &UnixConfigResponse::Failed {
            operation: operation.to_string(),
            message,
        },
        None,
    )
}

fn combine_results(
    primary: Result<(), GatewayError>,
    cleanup: Result<(), GatewayError>,
    operation: &'static str,
) -> Result<(), GatewayError> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(GatewayError::Platform {
            operation,
            message: format!("{primary}; cleanup failed: {cleanup}"),
        }),
    }
}

struct SocketGuard {
    path: PathBuf,
    armed: bool,
}

impl SocketGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn cleanup(&mut self) -> Result<(), GatewayError> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                ensure_socket(&self.path, metadata.file_type())?;
                std::fs::remove_file(&self.path)
                    .map_err(|error| GatewayError::platform("remove-unix-helper-socket", error))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(GatewayError::platform("inspect-unix-helper-socket", error)),
        }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::os::unix::net::UnixStream;

    use nix::unistd::Uid;

    use super::peer_uid;
    use super::remove_stale_socket;
    use super::secure_child_path;

    #[test]
    fn stale_cleanup_refuses_regular_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("helper.sock");
        std::fs::write(&path, b"not a socket").expect("write fixture");
        assert!(remove_stale_socket(&path).is_err());
        assert!(path.exists());
    }

    #[test]
    fn stale_cleanup_removes_only_socket_node() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("helper.sock");
        let listener = UnixListener::bind(&path).expect("bind fixture");
        drop(listener);
        remove_stale_socket(&path).expect("remove socket");
        assert!(!path.exists());
    }

    #[test]
    fn peer_uid_matches_the_connecting_process() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("helper.sock");
        let listener = UnixListener::bind(&path).expect("bind fixture");
        let client_path = path.clone();
        let client =
            std::thread::spawn(move || UnixStream::connect(client_path).expect("connect fixture"));
        let (server, _) = listener.accept().expect("accept fixture");
        assert_eq!(
            peer_uid(&server).expect("read peer credentials"),
            Uid::effective().as_raw()
        );
        client.join().expect("client thread");
    }

    #[test]
    fn privileged_child_path_rejects_group_writable_ancestor() {
        let directory = tempfile::tempdir().expect("temporary directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o770))
            .expect("make fixture group writable");
        let child = directory.path().join("private");
        std::fs::create_dir(&child).expect("create private child");
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700))
            .expect("make child private");
        let path = child.join("routes.json");
        let error = secure_child_path(&path, Uid::effective().as_raw(), "test-path")
            .expect_err("group-writable ancestor must be rejected");
        assert!(error.to_string().contains("unsafe group/other write bits"));
    }
}
