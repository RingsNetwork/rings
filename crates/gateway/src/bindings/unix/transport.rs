//! Length-delimited JSON and SCM_RIGHTS transport for the Unix helper.

use std::io::IoSlice;
use std::io::IoSliceMut;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;

use nix::sys::socket::recvmsg;
use nix::sys::socket::sendmsg;
use nix::sys::socket::ControlMessage;
use nix::sys::socket::ControlMessageOwned;
use nix::sys::socket::MsgFlags;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::config::UnixConfigRequest;
use super::config::UnixConfigResponse;
use crate::GatewayError;

const MAX_FRAME_BYTES: usize = 64 * 1024;
const RESPONSE_MARKER: u8 = 0x52;

pub(crate) fn write_request(
    stream: &mut UnixStream,
    request: &UnixConfigRequest,
) -> Result<(), GatewayError> {
    write_frame(stream, request, "write-helper-request")
}

pub(crate) fn read_request(
    stream: &mut UnixStream,
) -> Result<Option<UnixConfigRequest>, GatewayError> {
    read_optional_frame(stream, "read-helper-request")
}

pub(crate) fn send_response(
    stream: &mut UnixStream,
    response: &UnixConfigResponse,
    descriptor: Option<RawFd>,
) -> Result<(), GatewayError> {
    let marker = [RESPONSE_MARKER];
    let slices = [IoSlice::new(&marker)];
    let rights = descriptor.map(|fd| [fd]);
    let sent = match rights.as_ref() {
        Some(fds) => sendmsg::<()>(
            stream.as_raw_fd(),
            &slices,
            &[ControlMessage::ScmRights(fds)],
            MsgFlags::empty(),
            None,
        ),
        None => sendmsg::<()>(stream.as_raw_fd(), &slices, &[], MsgFlags::empty(), None),
    }
    .map_err(|error| GatewayError::platform("send-helper-response-marker", error))?;
    if sent != marker.len() {
        return Err(GatewayError::Platform {
            operation: "send-helper-response-marker",
            message: format!("sent {sent} bytes for a {} byte marker", marker.len()),
        });
    }
    write_frame(stream, response, "write-helper-response")
}

pub(crate) fn receive_response(
    stream: &mut UnixStream,
) -> Result<(UnixConfigResponse, Option<OwnedFd>), GatewayError> {
    let mut marker = [0_u8; 1];
    let mut control = nix::cmsg_space!([RawFd; 1]);
    let (received_bytes, mut received) = {
        let mut slices = [IoSliceMut::new(&mut marker)];
        let message = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut slices,
            Some(&mut control),
            MsgFlags::empty(),
        )
        .map_err(|error| GatewayError::platform("receive-helper-response-marker", error))?;
        let mut received = Vec::new();
        for message in message
            .cmsgs()
            .map_err(|error| GatewayError::platform("decode-helper-control-message", error))?
        {
            if let ControlMessageOwned::ScmRights(descriptors) = message {
                for descriptor in descriptors {
                    // SAFETY: each descriptor is newly installed in this process by `recvmsg` and
                    // SCM_RIGHTS transfers ownership to the receiver. Each value is wrapped once.
                    received.push(unsafe { OwnedFd::from_raw_fd(descriptor) });
                }
            }
        }
        (message.bytes, received)
    };
    if received_bytes != marker.len() {
        return Err(GatewayError::Platform {
            operation: "receive-helper-response-marker",
            message: format!(
                "received {} bytes for a {} byte marker",
                received_bytes,
                marker.len()
            ),
        });
    }
    if marker.first().copied() != Some(RESPONSE_MARKER) {
        return Err(GatewayError::Platform {
            operation: "receive-helper-response-marker",
            message: "helper returned an invalid response marker".to_string(),
        });
    }

    let descriptor = received.drain(..).next();
    drop(received);
    let response = read_required_frame(stream, "read-helper-response")?;
    Ok((response, descriptor))
}

fn write_frame<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
    operation: &'static str,
) -> Result<(), GatewayError> {
    let payload =
        serde_json::to_vec(value).map_err(|error| GatewayError::platform(operation, error))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(GatewayError::Platform {
            operation,
            message: format!(
                "helper frame is {} bytes; limit is {MAX_FRAME_BYTES}",
                payload.len()
            ),
        });
    }
    let length =
        u32::try_from(payload.len()).map_err(|error| GatewayError::platform(operation, error))?;
    stream
        .write_all(&length.to_be_bytes())
        .and_then(|()| stream.write_all(&payload))
        .map_err(|error| GatewayError::platform(operation, error))
}

fn read_optional_frame<T: DeserializeOwned>(
    stream: &mut UnixStream,
    operation: &'static str,
) -> Result<Option<T>, GatewayError> {
    let mut length = [0_u8; 4];
    let first = stream
        .read(&mut length)
        .map_err(|error| GatewayError::platform(operation, error))?;
    if first == 0 {
        return Ok(None);
    }
    stream
        .read_exact(
            length
                .get_mut(first..)
                .ok_or_else(|| GatewayError::Platform {
                    operation,
                    message: format!("invalid frame prefix length {first}"),
                })?,
        )
        .map_err(|error| GatewayError::platform(operation, error))?;
    decode_frame(stream, length, operation).map(Some)
}

fn read_required_frame<T: DeserializeOwned>(
    stream: &mut UnixStream,
    operation: &'static str,
) -> Result<T, GatewayError> {
    read_optional_frame(stream, operation)?.ok_or_else(|| GatewayError::Platform {
        operation,
        message: "helper control connection closed before the next frame".to_string(),
    })
}

fn decode_frame<T: DeserializeOwned>(
    stream: &mut UnixStream,
    length: [u8; 4],
    operation: &'static str,
) -> Result<T, GatewayError> {
    let length = usize::try_from(u32::from_be_bytes(length))
        .map_err(|error| GatewayError::platform(operation, error))?;
    if length > MAX_FRAME_BYTES {
        return Err(GatewayError::Platform {
            operation,
            message: format!("helper frame is {length} bytes; limit is {MAX_FRAME_BYTES}"),
        });
    }
    let mut payload = vec![0_u8; length];
    stream
        .read_exact(&mut payload)
        .map_err(|error| GatewayError::platform(operation, error))?;
    serde_json::from_slice(&payload).map_err(|error| GatewayError::platform(operation, error))
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    use super::read_request;
    use super::receive_response;
    use super::send_response;
    use super::write_request;
    use crate::bindings::unix::config::UnixConfigRequest;
    use crate::bindings::unix::config::UnixConfigResponse;
    use crate::GatewayPlan;
    use crate::Mtu;

    fn plan() -> GatewayPlan {
        GatewayPlan {
            addresses: vec!["100.64.0.1/32".parse().expect("address")],
            included_routes: vec!["203.0.113.0/24".parse().expect("route")],
            mtu: Mtu::try_from(1_280).expect("MTU"),
        }
    }

    #[test]
    fn request_round_trip_preserves_the_validated_plan() {
        let (mut server, mut client) = UnixStream::pair().expect("pair");
        let request = UnixConfigRequest::Establish { plan: plan() };
        write_request(&mut client, &request).expect("write request");
        assert_eq!(
            read_request(&mut server).expect("read request"),
            Some(request)
        );
    }

    #[test]
    fn response_round_trip_transfers_one_owned_descriptor() {
        let (mut server, mut client) = UnixStream::pair().expect("pair");
        let descriptor = File::open("/dev/null").expect("open /dev/null");
        let sender = std::thread::spawn(move || {
            send_response(
                &mut server,
                &UnixConfigResponse::Established {
                    lease_id: "lease-1".to_string(),
                    interface_name: "tun-test".to_string(),
                },
                Some(descriptor.as_raw_fd()),
            )
            .expect("send response");
        });

        let (response, received) = receive_response(&mut client).expect("receive response");
        sender.join().expect("sender join");
        assert_eq!(response, UnixConfigResponse::Established {
            lease_id: "lease-1".to_string(),
            interface_name: "tun-test".to_string(),
        });
        assert!(received.is_some());
    }
}
