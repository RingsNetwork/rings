//! Unprivileged client for the foreground Unix tunnel-configuration helper.

use std::net::IpAddr;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use super::config::UnixConfigRequest;
use super::config::UnixConfigResponse;
use super::lease::UnixLeaseId;
use super::transport::receive_response;
use super::transport::write_request;
use crate::bindings::normalize_underlay_targets;
use crate::bindings::EstablishedTunnel;
use crate::bindings::NativePacketIo;
use crate::bindings::TeardownFailure;
use crate::bindings::TunnelControl;
use crate::bindings::UnderlayPolicy;
use crate::GatewayError;
use crate::GatewayPlan;

const DEFAULT_UNIX_HELPER_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_UNIX_HELPER_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Connection options for the unprivileged Unix gateway client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnixTunnelOptions {
    /// Filesystem path of the already-running foreground helper socket.
    pub socket_path: PathBuf,
    /// Maximum duration of one helper connection or request/response exchange.
    pub control_timeout: Duration,
}

impl UnixTunnelOptions {
    /// Select the helper socket used for one gateway lifecycle.
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            control_timeout: DEFAULT_UNIX_HELPER_CONTROL_TIMEOUT,
        }
    }

    /// Override the helper control-operation deadline.
    pub fn with_control_timeout(mut self, control_timeout: Duration) -> Self {
        self.control_timeout = control_timeout;
        self
    }
}

/// Linear cleanup capability returned by the Unix helper.
pub struct UnixTunnelLease {
    id: UnixLeaseId,
}

/// Privilege-separated Unix tunnel controller.
///
/// A single persistent connection covers establishment, bypass refreshes, and teardown. If this
/// connection disappears early, the foreground helper retains the active interface and routes;
/// constructing a new controller with the same plan resumes that fail-closed lease.
pub struct UnixTunnelControl {
    options: UnixTunnelOptions,
    underlay_targets: Vec<IpAddr>,
    active: Option<UnixLeaseId>,
    stream: Option<Arc<Mutex<UnixStream>>>,
}

impl UnixTunnelControl {
    /// Construct a client without connecting to or mutating the host.
    pub fn new(options: UnixTunnelOptions) -> Self {
        Self {
            options,
            underlay_targets: Vec::new(),
            active: None,
            stream: None,
        }
    }

    async fn establish_via_helper(
        &self,
        request: UnixConfigRequest,
    ) -> Result<HelperReply, GatewayError> {
        let socket_path = self.options.socket_path.clone();
        let control_timeout = self.validated_control_timeout()?;
        let stream = tokio::time::timeout(
            control_timeout,
            tokio::net::UnixStream::connect(&socket_path),
        )
        .await
        .map_err(|_| control_timeout_error("connect-unix-helper", control_timeout))?
        .map_err(|error| GatewayError::Platform {
            operation: "connect-unix-helper",
            message: format!("{}: {error}", socket_path.display()),
        })?
        .into_std()
        .map_err(|error| GatewayError::Platform {
            operation: "convert-unix-helper-stream",
            message: error.to_string(),
        })?;
        configure_control_stream(&stream, control_timeout)?;
        let task = tokio::task::spawn_blocking(move || {
            let mut stream = stream;
            write_request(&mut stream, &request)?;
            let (response, descriptor) = receive_response(&mut stream)?;
            Ok(HelperReply {
                stream: Arc::new(Mutex::new(stream)),
                response,
                descriptor,
            })
        });
        tokio::time::timeout(control_timeout, task)
            .await
            .map_err(|_| control_timeout_error("exchange-unix-helper-establish", control_timeout))?
            .map_err(|error| GatewayError::Platform {
                operation: "join-unix-helper-establish",
                message: error.to_string(),
            })?
    }

    async fn exchange(
        &self,
        request: UnixConfigRequest,
    ) -> Result<(UnixConfigResponse, Option<std::os::fd::OwnedFd>), GatewayError> {
        let stream = self
            .stream
            .as_ref()
            .cloned()
            .ok_or_else(|| GatewayError::Platform {
                operation: "exchange-unix-helper",
                message: "Unix helper control connection is not active".to_string(),
            })?;
        let control_timeout = self.validated_control_timeout()?;
        let task = tokio::task::spawn_blocking(move || {
            let mut stream = match stream.lock() {
                Ok(stream) => stream,
                Err(poisoned) => poisoned.into_inner(),
            };
            write_request(&mut stream, &request)?;
            receive_response(&mut stream)
        });
        tokio::time::timeout(control_timeout, task)
            .await
            .map_err(|_| control_timeout_error("exchange-unix-helper", control_timeout))?
            .map_err(|error| GatewayError::Platform {
                operation: "join-unix-helper-exchange",
                message: error.to_string(),
            })?
    }

    fn validated_control_timeout(&self) -> Result<Duration, GatewayError> {
        if self.options.control_timeout.is_zero() {
            return Err(GatewayError::Platform {
                operation: "validate-unix-helper-timeout",
                message: "Unix helper control timeout must be greater than zero".to_string(),
            });
        }
        if self.options.control_timeout > MAX_UNIX_HELPER_CONTROL_TIMEOUT {
            return Err(GatewayError::Platform {
                operation: "validate-unix-helper-timeout",
                message: format!(
                    "Unix helper control timeout must not exceed {MAX_UNIX_HELPER_CONTROL_TIMEOUT:?}"
                ),
            });
        }
        Ok(self.options.control_timeout)
    }
}

fn configure_control_stream(
    stream: &UnixStream,
    control_timeout: Duration,
) -> Result<(), GatewayError> {
    stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_read_timeout(Some(control_timeout)))
        .and_then(|()| stream.set_write_timeout(Some(control_timeout)))
        .map_err(|error| GatewayError::Platform {
            operation: "configure-unix-helper-stream",
            message: error.to_string(),
        })
}

fn control_timeout_error(operation: &'static str, timeout: Duration) -> GatewayError {
    GatewayError::Platform {
        operation,
        message: format!("Unix helper control operation timed out after {timeout:?}"),
    }
}

struct HelperReply {
    stream: Arc<Mutex<UnixStream>>,
    response: UnixConfigResponse,
    descriptor: Option<std::os::fd::OwnedFd>,
}

#[async_trait::async_trait]
impl TunnelControl for UnixTunnelControl {
    type Device = NativePacketIo;
    type Lease = UnixTunnelLease;

    async fn establish(
        &mut self,
        plan: &GatewayPlan,
    ) -> Result<EstablishedTunnel<Self::Device, Self::Lease>, GatewayError> {
        if self.active.is_some() || self.stream.is_some() {
            return Err(GatewayError::Platform {
                operation: "establish-unix-helper",
                message: "a Unix helper tunnel lease is already active".to_string(),
            });
        }
        let reply = self
            .establish_via_helper(UnixConfigRequest::Establish {
                plan: plan.clone(),
                underlay_targets: self.underlay_targets.clone(),
            })
            .await?;
        let (lease_id, interface_name) = match reply.response {
            UnixConfigResponse::Established {
                lease_id,
                interface_name,
            } => (lease_id, interface_name),
            UnixConfigResponse::Failed { operation, message } => {
                return Err(helper_failure(operation, message));
            }
            response => return Err(unexpected_response("establish-unix-helper", response)),
        };
        let lease_id = UnixLeaseId::new(lease_id).ok_or_else(|| GatewayError::Platform {
            operation: "establish-unix-helper",
            message: "helper returned an empty lease identifier".to_string(),
        })?;
        let descriptor = reply.descriptor.ok_or_else(|| GatewayError::Platform {
            operation: "establish-unix-helper",
            message: "helper established a tunnel without transferring its packet descriptor"
                .to_string(),
        })?;
        let device = NativePacketIo::from_owned_fd(descriptor)?;
        self.stream = Some(reply.stream);
        self.active = Some(lease_id.clone());
        Ok(EstablishedTunnel {
            device,
            lease: UnixTunnelLease { id: lease_id },
            interface_name,
        })
    }

    async fn teardown(&mut self, lease: Self::Lease) -> Result<(), TeardownFailure<Self::Lease>> {
        let Some(active) = self.active.as_ref() else {
            return Err(TeardownFailure::new(lease, GatewayError::Platform {
                operation: "teardown-unix-helper",
                message: "no Unix helper tunnel lease is active".to_string(),
            }));
        };
        if active != &lease.id {
            let error = GatewayError::Platform {
                operation: "teardown-unix-helper",
                message: format!(
                    "lease {} does not own active Unix helper lease {}",
                    lease.id.as_str(),
                    active.as_str()
                ),
            };
            return Err(TeardownFailure::new(lease, error));
        }
        let exchange = self
            .exchange(UnixConfigRequest::Teardown {
                lease_id: lease.id.as_str().to_string(),
            })
            .await;
        let (response, descriptor) = match exchange {
            Ok(reply) => reply,
            Err(error) => return Err(TeardownFailure::new(lease, error)),
        };
        if descriptor.is_some() {
            return Err(TeardownFailure::new(lease, GatewayError::Platform {
                operation: "teardown-unix-helper",
                message: "helper transferred an unexpected descriptor during teardown".to_string(),
            }));
        }
        match response {
            UnixConfigResponse::TornDown => {
                self.active = None;
                self.stream = None;
                Ok(())
            }
            UnixConfigResponse::Failed { operation, message } => Err(TeardownFailure::new(
                lease,
                helper_failure(operation, message),
            )),
            response => Err(TeardownFailure::new(
                lease,
                unexpected_response("teardown-unix-helper", response),
            )),
        }
    }
}

#[async_trait::async_trait]
impl UnderlayPolicy for UnixTunnelControl {
    async fn replace_bypass_targets(&mut self, targets: &[IpAddr]) -> Result<(), GatewayError> {
        let normalized = normalize_underlay_targets(targets)?;
        let Some(active) = self.active.as_ref() else {
            self.underlay_targets = normalized;
            return Ok(());
        };
        let (response, descriptor) = self
            .exchange(UnixConfigRequest::ReplaceBypass {
                lease_id: active.as_str().to_string(),
                underlay_targets: normalized.clone(),
            })
            .await?;
        if descriptor.is_some() {
            return Err(GatewayError::Platform {
                operation: "replace-unix-helper-bypass",
                message: "helper transferred an unexpected descriptor during a bypass update"
                    .to_string(),
            });
        }
        match response {
            UnixConfigResponse::Updated => {
                self.underlay_targets = normalized;
                Ok(())
            }
            UnixConfigResponse::Failed { operation, message } => {
                Err(helper_failure(operation, message))
            }
            response => Err(unexpected_response("replace-unix-helper-bypass", response)),
        }
    }
}

fn helper_failure(operation: String, message: String) -> GatewayError {
    GatewayError::Platform {
        operation: "unix-helper-request",
        message: format!("{operation}: {message}"),
    }
}

fn unexpected_response(operation: &'static str, response: UnixConfigResponse) -> GatewayError {
    GatewayError::Platform {
        operation,
        message: format!("helper returned unexpected response {response:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::UnixTunnelControl;
    use super::UnixTunnelOptions;
    use crate::bindings::UnderlayPolicy;

    #[tokio::test]
    async fn bypass_targets_are_normalized_before_connection() {
        let mut control = UnixTunnelControl::new(UnixTunnelOptions::new(PathBuf::from(
            "/tmp/rings-gateway-test-unused.sock",
        )));
        let target = IpAddr::from([192, 0, 2, 4]);
        control
            .replace_bypass_targets(&[target, target])
            .await
            .expect("replace bypass");
        assert_eq!(control.underlay_targets, vec![target]);
    }

    #[tokio::test]
    async fn stalled_helper_exchange_is_bounded() {
        let (client, _stalled_helper) =
            std::os::unix::net::UnixStream::pair().expect("create Unix helper stream pair");
        let timeout = Duration::from_millis(25);
        super::configure_control_stream(&client, timeout).expect("configure client stream");
        let mut control = UnixTunnelControl::new(
            UnixTunnelOptions::new(PathBuf::from("/unused")).with_control_timeout(timeout),
        );
        control.stream = Some(std::sync::Arc::new(std::sync::Mutex::new(client)));

        let started = tokio::time::Instant::now();
        let result = control
            .exchange(super::UnixConfigRequest::Teardown {
                lease_id: "test-lease".to_string(),
            })
            .await;

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn helper_timeout_rejects_zero_and_effectively_unbounded_values() {
        let zero = UnixTunnelControl::new(
            UnixTunnelOptions::new(PathBuf::from("/unused")).with_control_timeout(Duration::ZERO),
        );
        assert!(zero.validated_control_timeout().is_err());

        let too_large = UnixTunnelControl::new(
            UnixTunnelOptions::new(PathBuf::from("/unused"))
                .with_control_timeout(Duration::from_secs(31)),
        );
        assert!(too_large.validated_control_timeout().is_err());
    }
}
